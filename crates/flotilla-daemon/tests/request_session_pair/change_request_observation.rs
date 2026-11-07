use flotilla_change_request::observation::{ChangeRequestKey, ChangeRequestObservationSource};
use flotilla_observation::change_request_observer::{ChangeRequestRefreshCadence, ChangeRequestRefresher};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{
    promises::{self, PromiseKind, PromiseOperation, PromiseSource, PromiseState, Submission},
    ChangeRequest, ChangeRequestLeafSubject, ChangeRequestStatus, ConvoyStatus, IssueSourceBindingSpec, LeafSubject, Observation,
    ObservedChangeRequestState, ObservedChecks, Project, ProjectSpec,
};
use tokio_util::task::AbortOnDropHandle;

use super::*;

// External forge boundary: the owner sees complete observations; any nonowner
// read is a failure. Transport, replication, convoys and observers are real.
struct ForgeObservation(tokio::sync::Mutex<ObservedChangeRequestState>);
#[async_trait]
impl ChangeRequestObservationSource for ForgeObservation {
    async fn observe(&self, _: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
        Ok(ChangeRequestStatus {
            state: Observation::known(*self.0.lock().await, Utc::now()),
            checks: Observation::known(ObservedChecks::Pass, Utc::now()),
            head_sha: Observation::known("head".into(), Utc::now()),
            title: Observation::default(),
            author: Observation::default(),
            review_decision: Observation::default(),
            review_requested_from_owner: Observation::default(),
            review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, Utc::now()) },
            mergeable: Observation::known(flotilla_resources::ObservedMergeability::Mergeable, Utc::now()),
        })
    }
}
struct ForbiddenForge(std::sync::atomic::AtomicUsize);
#[async_trait]
impl ChangeRequestObservationSource for ForbiddenForge {
    async fn observe(&self, _: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(ObservationError::Failed("nonowner contacted forge".into()))
    }
}

// #3030: remote convoy demand must reach the source owner and replicate
// state/checks/readiness that can keep a PR promise.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn remote_produced_change_request_is_owner_observed() {
    remote_change_request_scenario(false).await;
}

// The requester's demand writes nothing of its own: it reads the record the source's
// observer writes, replicated from the observer's home, even when the demand arrives
// before the observer watches for it.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn new_remote_change_request_names_source_owner() {
    remote_change_request_scenario(true).await;
}

// Generated schedules cover demand arriving before/after watcher startup, through the
// same routed replication scenario.
#[hegel::test]
fn generated_remote_change_request_observation(tc: hegel::TestCase) {
    let late_watcher = tc.draw(gs::booleans());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime")
        .block_on(remote_change_request_scenario(late_watcher));
}

/// The homes holding a copy of ChangeRequest `name`, as `daemon` reads them.
async fn change_request_homes(daemon: &InProcessDaemon, name: &str) -> Vec<String> {
    let local = daemon.resource_backend().local_root().expect("local root").to_string();
    let sources = daemon.resource_backend().including_replicas::<ChangeRequest>("flotilla").list_replica_sources().await.expect("sources");
    sources
        .items
        .into_iter()
        .filter(|source| source.object.metadata.name == name)
        .map(|source| match source.provenance {
            flotilla_resources::ResourceProvenance::Local => local.clone(),
            flotilla_resources::ResourceProvenance::Replica { origin_root, .. } => origin_root.to_string(),
        })
        .collect()
}

async fn remote_change_request_scenario(late_watcher: bool) {
    let owner = empty_daemon_named("cr-owner").await;
    let requester = empty_daemon_named("cr-requester").await;
    let source = IssueSource { service: "github.com".into(), scope: "org/shared".into() };
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
        .unwrap();
    // Only a host that has sent a heartbeat is a candidate observer: the owner has.
    let owner_host = owner.local_host_id().expect("owner host identity").to_string();
    let hosts = owner.resource_backend().using::<Host>("flotilla");
    let host = hosts.create(&InputMeta::builder().name(owner_host.clone()).build(), &HostSpec::default()).await.expect("owner host");
    hosts
        .update_status(
            &owner_host,
            &host.metadata.resource_version,
            &HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..HostStatus::default() },
        )
        .await
        .expect("owner heartbeat");
    let mesh = spawn_in_memory_request_mesh_with_replication_kinds(
        vec![owner.clone(), requester.clone()],
        Some(&[Project::API_PATHS.kind, Host::API_PATHS.kind, Convoy::API_PATHS.kind, ChangeRequest::API_PATHS.kind]),
    )
    .await
    .unwrap();
    eventually(Duration::from_secs(5), Duration::from_millis(10), "source owner replicated", || async {
        flotilla_observation::forge_observation::source_owner(&requester.resource_backend(), "flotilla", &source).await
            == Ok(Some(owner.node_id().clone()))
    })
    .await;
    let subject = ChangeRequestKey::new(
        "flotilla",
        flotilla_resources::address::stored_change_request_address(&source.service.clone(), &source.scope.clone(), 330, &[])
            .expect("address"),
    )
    .expect("key");
    let name = flotilla_resources::address::record_name(
        &flotilla_resources::address::stored_change_request_address(&source.service, &source.scope, 330, &[])
            .expect("change request address"),
    )
    .expect("record name");
    let forge = Arc::new(ForgeObservation(tokio::sync::Mutex::new(ObservedChangeRequestState::Open)));
    // Each observer writes to its daemon's engine records, as composition installs them.
    let refresher = |daemon: &Arc<InProcessDaemon>, source: Arc<dyn ChangeRequestObservationSource>| {
        let refresher = ChangeRequestRefresher::new(
            daemon.host_name().to_string(),
            daemon.resource_backend(),
            daemon.node_id().to_string(),
            source,
            ChangeRequestRefreshCadence::default(),
        );
        let records = flotilla_provisioning::engine::observations::EngineObservations::open(
            daemon.decided_store().expect("decided store"),
            Arc::new(flotilla_resources::SystemClock),
        )
        .expect("engine observations");
        refresher.engine().install(Arc::new(records)).expect("observer on the engine");
        refresher
    };
    let owner_observer = refresher(&owner, forge.clone());
    let forbidden = Arc::new(ForbiddenForge(std::sync::atomic::AtomicUsize::new(0)));
    let requester_observer = refresher(&requester, forbidden.clone());
    let start_watcher = || {
        let watcher = owner_observer.clone();
        AbortOnDropHandle::new(tokio::spawn(async move { watcher.watch_convoy_demands("flotilla").await }))
    };
    let mut owner_demand_task = (!late_watcher).then(start_watcher);
    tokio::task::yield_now().await;
    let mut status = ConvoyStatus::default();
    promises::apply(
        &mut status,
        "work",
        "coder",
        &PromiseOperation::Submit {
            id: "produced-pr".into(),
            kind: PromiseKind::Pr,
            source: PromiseSource::Crew,
            submission: Submission {
                reference: "cr/github.com/org/shared/330".into(),
                metadata: BTreeMap::new(),
                submitted_at: Utc::now(),
                verdict: None,
            },
        },
    );
    let convoys = requester.resource_backend().using::<Convoy>("flotilla");
    let convoy = convoys
        .create(&InputMeta::builder().name("producer".into()).build(), &ConvoySpec::builder().workflow_ref("scratch".into()).build())
        .await
        .unwrap();
    convoys.update_status("producer", &convoy.metadata.resource_version, &status).await.unwrap();
    requester_observer.demand(uuid::Uuid::new_v4(), subject.clone(), None).await.unwrap();
    assert!(
        !change_request_homes(&requester, &name).await.contains(&requester.node_id().to_string()),
        "a requester that does not observe the source writes no record of its own"
    );
    if late_watcher {
        eventually(Duration::from_secs(5), Duration::from_millis(10), "convoy replicated before observer startup", || async {
            owner.resource_backend().including_replicas::<Convoy>("flotilla").get("producer").await.is_ok()
        })
        .await;
        owner_demand_task = Some(start_watcher());
    }
    eventually(Duration::from_secs(5), Duration::from_millis(10), "owner observation reaches producer", || async {
        requester.resource_backend().including_replicas::<ChangeRequest>("flotilla").get_all(&name).await.unwrap().items.iter().any(
            |copy| {
                copy.object.spec.observing_authority == owner.node_id().to_string()
                    && copy.object.status.as_ref().is_some_and(|s| {
                        s.state.value == Some(ObservedChangeRequestState::Open) && s.checks.value == Some(ObservedChecks::Pass)
                    })
            },
        )
    })
    .await;
    let copies = requester.resource_backend().including_replicas::<ChangeRequest>("flotilla").list().await.unwrap();
    let record = flotilla_resources::select_change_requests(copies.items.iter().map(|copy| &copy.object))
        .into_values()
        .next()
        .expect("selected owner observation");
    assert_eq!(
        change_request_homes(&requester, &name).await,
        vec![owner.node_id().to_string()],
        "the requester reads the observer's record, replicated from its home"
    );
    assert_eq!(
        ChangeRequestLeafSubject { change_request: &record, now: Utc::now(), stale_after: Duration::from_secs(180) }
            .value(".ready")
            .unwrap()
            .to_string(),
        "true"
    );
    *forge.0.lock().await = ObservedChangeRequestState::Merged;
    owner_observer.refresh_demanded_owned().await.unwrap();
    eventually(Duration::from_secs(5), Duration::from_millis(10), "merged observation keeps promise", || async {
        let copies = requester.resource_backend().including_replicas::<ChangeRequest>("flotilla").list().await.unwrap();
        let records = flotilla_resources::select_change_requests(copies.items.iter().map(|copy| &copy.object))
            .into_values()
            .map(|record| (name.clone(), record))
            .collect();
        let Some(patch) =
            promises::next_observation(&status, "producer", &records, &BTreeMap::new(), &[], Utc::now(), Duration::from_secs(180))
        else {
            return false;
        };
        let kept = flotilla_store::apply_status_patch(&convoys, "producer", &patch).await.expect("persist observed promise verdict");
        kept.status.expect("producer status").promises["work"]["coder"][0].state == PromiseState::Kept
    })
    .await;
    // Duplicate convoy consumers share one observer demand. Removing one must
    // preserve the other; removing the last releases the standing demand.
    let duplicate =
        convoys.create(&InputMeta::builder().name("second-producer".into()).build(), &convoy.spec).await.expect("second consumer");
    convoys.update_status("second-producer", &duplicate.metadata.resource_version, &status).await.expect("second subject");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "second consumer replicated", || async {
        owner
            .resource_backend()
            .including_replicas::<Convoy>("flotilla")
            .get("second-producer")
            .await
            .is_ok_and(|copy| copy.object.status.is_some())
    })
    .await;
    convoys.delete("producer").await.expect("remove first consumer");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "first consumer deletion replicated", || async {
        owner.resource_backend().including_replicas::<Convoy>("flotilla").get("producer").await.is_err()
    })
    .await;
    assert_eq!(owner_observer.demanded_owned().await.expect("remaining owner demand"), vec![subject]);
    convoys.delete("second-producer").await.expect("remove last consumer");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "last consumer releases demand", || async {
        owner_observer.demanded_owned().await.is_ok_and(|subjects| subjects.is_empty())
    })
    .await;
    drop(owner_demand_task.expect("owner watcher"));
    assert_eq!(forbidden.0.load(std::sync::atomic::Ordering::SeqCst), 0, "nonowner never reads the forge");
    drop(mesh);
}
