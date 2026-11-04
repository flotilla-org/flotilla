use flotilla_store_memory::InMemoryBackend;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_change_request::observation::ChangeRequestKey;
use flotilla_change_request::ChangeRequestTracker;
use flotilla_config::ConfigStore;
use flotilla_protocol::HostName;
use flotilla_resources::{
    ConvoyRepositorySpec, Host as ResourceHost, HostSpec, HostStatus, Project, ProjectSpec, Repository, RepositorySpec,
};
use flotilla_store::ResourceBackend;

use super::observation_support::{rest_admission_fixture, RestAdmissionLookup, RestAdmissionReply};
use super::support::{test_meta, SuspendedBoardProvider};
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::{fake_discovery_with_provider_set, FakeChangeRequest, FakeDiscoveryProviders};
use flotilla_observation::forge_observation::OWNER_LOCAL_INCREMENTAL_READ_ERROR;
use flotilla_protocol::ChangeRequest;

// #2868: three daemons sharing a Project observe its change requests at one owner;
// explicit observation withdrawal elects one replacement, and recovery restores the home.
// Board and issue reads are per host: each daemon calls its own forge once per freshness window.
struct CountingForgeRequests(AtomicUsize);
#[async_trait]
impl ChangeRequestTracker for CountingForgeRequests {
    async fn list_change_requests(&self, _limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        unreachable!()
    }
    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok((
            id.into(),
            ChangeRequest {
                title: "Shared PR".into(),
                branch: "work".into(),
                status: flotilla_protocol::ChangeRequestStatus::Open,
                body: None,
                provider_name: "fake".into(),
                provider_display_name: "Fake".into(),
            },
        ))
    }
    async fn open_in_browser(&self, _id: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn close_change_request(&self, _id: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn merge_change_request(&self, _id: &str) -> Result<(), String> {
        unreachable!()
    }
    async fn list_merged_branch_names(&self, _limit: usize) -> Result<Vec<String>, String> {
        unreachable!()
    }
}

#[tokio::test]
async fn three_host_forge_observation_has_one_owner_and_replicates_facts() {
    use flotilla_resources::Resource;
    let mut temps = Vec::new();
    let mut daemons = Vec::new();
    let mut providers = Vec::new();
    let mut request_providers = Vec::new();
    for name in ["owner", "second", "third"] {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("daemon.toml"), format!("machine_id = \"forge-{name}\"\n")).unwrap();
        let provider = Arc::new(SuspendedBoardProvider {
            calls: AtomicUsize::new(0),
            issue_calls: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(1000),
        });
        let requests = Arc::new(CountingForgeRequests(AtomicUsize::new(0)));
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery_with_provider_set(
                FakeDiscoveryProviders::default().with_issue_tracker(provider.clone()).with_change_request(requests.clone()),
            ),
            HostName::new(name),
            ResourceBackend::new(InMemoryBackend::default()),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        temps.push(temp);
        daemons.push(daemon);
        providers.push(provider);
        request_providers.push(requests);
    }
    let source = flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "org/shared".into() };
    let project = ProjectSpec::builder()
        .display_name("Shared".into())
        .issue_source_bindings(vec![flotilla_resources::IssueSourceBindingSpec::builder()
            .source(source.clone())
            .alias("shared".into())
            .build()])
        .build();
    daemons[0].resource_backend().using::<Project>("flotilla").create(&test_meta("shared"), &project).await.unwrap();
    daemons[1].resource_backend().using::<Project>("flotilla").create(&test_meta("also-shared"), &project).await.unwrap();
    async fn replicate<T: Resource>(daemons: &[Arc<InProcessDaemon>]) {
        for source in daemons {
            let backend = source.resource_backend();
            let listed = backend.using::<T>("flotilla").list().await.unwrap();
            for target in daemons {
                if Arc::ptr_eq(source, target) {
                    continue;
                }
                target
                    .resource_backend()
                    .replica_writer::<T>(backend.local_root().unwrap(), "flotilla")
                    .replace(&listed, Utc::now())
                    .await
                    .unwrap();
            }
        }
    }
    replicate::<Project>(&daemons).await;
    let repository = RepositorySpec::remote("https://github.com/org/shared").unwrap();
    daemons[0]
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository.key().to_string()), &repository)
        .await
        .unwrap();
    replicate::<Repository>(&daemons).await;
    let subject = ChangeRequestKey::new(
        "flotilla",
        flotilla_resources::address::stored_change_request_address("github.com", "org/shared", 7, &[]).expect("address"),
    )
    .expect("key");
    let refreshers = daemons
        .iter()
        .map(|daemon| {
            flotilla_observation::change_request_observer::ChangeRequestRefresher::new(
                "topology".into(),
                daemon.resource_backend(),
                daemon.resource_backend().local_root().unwrap().to_string(),
                daemon.change_request_observation_source.clone(),
                Default::default(),
            )
        })
        .collect::<Vec<_>>();

    for (index, daemon) in daemons.iter().enumerate() {
        let hosts = daemon.resource_backend().using::<ResourceHost>("flotilla");
        let host = hosts.create(&test_meta(&format!("host-{index}")), &HostSpec::default()).await.unwrap();
        if index == 0 {
            continue;
        }
        hosts
            .update_status(
                &host.metadata.name,
                &host.metadata.resource_version,
                &HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() },
            )
            .await
            .unwrap();
    }
    replicate::<ResourceHost>(&daemons).await;
    // A declared Host whose first heartbeat has not arrived is unknown, not
    // positive unready evidence. Ready replicas must not steal its source.
    for daemon in &daemons {
        assert_eq!(
            flotilla_observation::forge_observation::source_owner(&daemon.resource_backend(), "flotilla", &source).await.unwrap(),
            Some(daemons[0].resource_backend().local_root().unwrap())
        );
    }
    let hosts = daemons[0].resource_backend().using::<ResourceHost>("flotilla");
    let host = hosts.get("host-0").await.unwrap();
    hosts
        .update_status(
            "host-0",
            &host.metadata.resource_version,
            &HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() },
        )
        .await
        .unwrap();
    replicate::<ResourceHost>(&daemons).await;
    for refresher in &refreshers {
        refresher.refresh_once(&subject).await.unwrap();
    }
    assert_eq!(request_providers.iter().map(|p| p.0.load(Ordering::SeqCst)).collect::<Vec<_>>(), vec![1, 0, 0]);
    replicate::<flotilla_resources::ChangeRequest>(&daemons).await;
    for daemon in &daemons {
        let record = daemon
            .resource_backend()
            .including_replicas::<flotilla_resources::ChangeRequest>("flotilla")
            .get(&subject.record_name())
            .await
            .unwrap();
        assert_eq!(record.object.status.unwrap().title.value.as_deref(), Some("Shared PR"));
    }

    for daemon in &daemons {
        assert_eq!(
            flotilla_observation::forge_observation::source_owner(&daemon.resource_backend(), "flotilla", &source).await.unwrap(),
            Some(daemons[0].resource_backend().local_root().unwrap()),
            "Project sources: {:?}; Hosts: {:?}",
            daemon.resource_backend().including_replicas::<Project>("flotilla").list_replica_sources().await.unwrap(),
            daemon.resource_backend().including_replicas::<ResourceHost>("flotilla").list_replica_sources().await.unwrap()
        );
    }
    for _ in 0..2 {
        for daemon in &daemons {
            let provider = daemon.issue_provider_for_source(&source).await.unwrap();
            assert_eq!(provider.dispatch_board(&source).await.unwrap().issues.len(), 400);
        }
    }
    assert_eq!(providers.iter().map(|p| p.calls.load(Ordering::SeqCst)).collect::<Vec<_>>(), vec![1, 1, 1]);
    let reference = flotilla_protocol::IssueRef { source: source.clone(), id: "1".into() };
    for _ in 0..2 {
        for daemon in &daemons {
            let provider = daemon.issue_provider_for_source(&source).await.unwrap();
            assert_eq!(provider.query(&source, &Default::default(), 1, 50).await.unwrap().items.len(), 1);
            assert_eq!(provider.fetch_by_id(&reference).await.unwrap().title, "Shared issue");
        }
    }
    assert_eq!(providers.iter().map(|p| p.issue_calls.load(Ordering::SeqCst)).collect::<Vec<_>>(), vec![2, 2, 2]);
    for (index, daemon) in daemons.iter().enumerate() {
        let provider = daemon.issue_provider_for_source(&source).await.unwrap();
        let changes = provider.list_changed_since(&source, "2026-10-01T00:00:00Z", 50).await;
        if index == 0 {
            assert_eq!(changes.unwrap().updated.len(), 1);
        } else {
            assert_eq!(changes.unwrap_err(), OWNER_LOCAL_INCREMENTAL_READ_ERROR);
        }
    }
    assert_eq!(providers.iter().map(|p| p.issue_calls.load(Ordering::SeqCst)).collect::<Vec<_>>(), vec![3, 2, 2]);
    let hosts = daemons[0].resource_backend().using::<ResourceHost>("flotilla");
    let host = hosts.get("host-0").await.unwrap();
    hosts
        .update_status(
            "host-0",
            &host.metadata.resource_version,
            &HostStatus {
                ready: true,
                heartbeat_at: Some(Utc::now()),
                conditions: vec![flotilla_resources::HostCondition::builder()
                    .condition_type("ForgeObservation")
                    .reason("Withdrawn")
                    .message("deliberate fleet observer handover")
                    .observed_at(Utc::now())
                    .value(flotilla_resources::ConditionValue::False)
                    .blocks_readiness(false)
                    // Explicit withdrawal must work independently of placement readiness.
                    .blocks_forge_observation(true)
                    .build()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    replicate::<ResourceHost>(&daemons).await;
    let expected = daemons[1..].iter().map(|daemon| daemon.resource_backend().local_root().unwrap()).min().unwrap();
    for daemon in &daemons {
        assert_eq!(
            flotilla_observation::forge_observation::source_owner(&daemon.resource_backend(), "flotilla", &source).await.unwrap(),
            Some(expected.clone())
        );
    }
    for refresher in &refreshers {
        refresher.refresh_once(&subject).await.unwrap();
    }
    assert_eq!(request_providers[0].0.load(Ordering::SeqCst), 1);
    assert_eq!(request_providers[1..].iter().map(|p| p.0.load(Ordering::SeqCst)).sum::<usize>(), 1);

    // Recovery restores the preferred observer; the fallback stops polling.
    let host = hosts.get("host-0").await.unwrap();
    hosts
        .update_status(
            "host-0",
            &host.metadata.resource_version,
            &HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() },
        )
        .await
        .unwrap();
    replicate::<ResourceHost>(&daemons).await;
    for daemon in &daemons {
        assert_eq!(
            flotilla_observation::forge_observation::source_owner(&daemon.resource_backend(), "flotilla", &source).await.unwrap(),
            Some(daemons[0].resource_backend().local_root().unwrap())
        );
    }
    for refresher in &refreshers {
        refresher.refresh_once(&subject).await.unwrap();
    }
    assert_eq!(request_providers[0].0.load(Ordering::SeqCst), 2);
    assert_eq!(request_providers[1..].iter().map(|p| p.0.load(Ordering::SeqCst)).sum::<usize>(), 1);
}

// #2873: fresh forge facts accept only open/draft continuation; a PR-free
// branch is valid, terminal requests require explicit reopening on the forge.
#[tokio::test]
async fn continuation_resolves_branch_without_pr_and_refuses_terminal_pr() {
    use flotilla_protocol::{ChangeRequestStatus, ConvoyContinuation};
    let fixture = rest_admission_fixture([RestAdmissionReply::Absent; 2], RestAdmissionLookup::Branch).await;
    let repository = ConvoyRepositorySpec {
        repo_ref: fixture.keys[0].clone(),
        url: "https://github.com/team/repo0".into(),
        source_ref: "main".into(),
        target_ref: "main".into(),
        workspace_slug: "repo0".into(),
        subpaths: vec![],
    };
    let (key, branch, request) = fixture
        .daemon
        .admission
        .resolve_continuation(std::slice::from_ref(&repository), &ConvoyContinuation::Branch("wip".into()))
        .await
        .expect("WIP continuation");
    assert_eq!(key, fixture.keys[0]);
    assert_eq!(branch, "wip");
    assert!(request.is_none());
    for status in [ChangeRequestStatus::Merged, ChangeRequestStatus::Closed] {
        let provider = Arc::new(FakeChangeRequest::new());
        provider
            .add_change_requests(vec![(
                "7".into(),
                ChangeRequest {
                    title: "Old PR".into(),
                    branch: "wip".into(),
                    status,
                    body: None,
                    provider_name: "fake".into(),
                    provider_display_name: "Fake".into(),
                },
            )])
            .await;
        fixture.daemon.admission.repository_change_requests().write().await.get_mut(&fixture.keys[0]).expect("provider").provider =
            provider;
        for input in [ConvoyContinuation::Branch("wip".into()), ConvoyContinuation::ChangeRequest("7".into())] {
            let error = fixture
                .daemon
                .admission
                .resolve_continuation(std::slice::from_ref(&repository), &input)
                .await
                .err()
                .expect("terminal refusal");
            assert!(error.contains("reopen"), "{error}");
        }
    }
}
