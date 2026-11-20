use flotilla_change_request::observation::{ChangeRequestKey, ChangeRequestObservationSource};
use flotilla_observation::change_request_observer::{ChangeRequestRefreshCadence, ChangeRequestRefresher};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{
    promises::{PromiseKind, PromiseOperation, PromiseSource, PromiseState, Submission},
    ChangeRequest, ChangeRequestLeafSubject, ChangeRequestStatus, IssueSourceBindingSpec, LeafSubject, Observation,
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

/// Admits convoy `name` at `requester`'s engine, its coder having submitted change request 330
/// of org/shared as the PR it promises. Returns the launch.
async fn produce(requester: &InProcessDaemon, name: &str) -> String {
    let launch = admit_convoy(
        requester,
        engine_convoy(
            name,
            ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("scratch".into()).build(),
            ConvoyStatus::default(),
        ),
    )
    .await;
    let convoy = requester.resource_backend().using::<Convoy>("flotilla").get(&launch).await.expect("admitted convoy");
    let submit = PromiseOperation::Submit {
        id: "produced-pr".into(),
        kind: PromiseKind::Pr,
        source: PromiseSource::Crew,
        submission: Submission {
            reference: "cr/github.com/org/shared/330".into(),
            metadata: BTreeMap::new(),
            submitted_at: Some(Utc::now()),
            verdict: None,
        },
    };
    let path = requester.convoy_engine_path();
    let engine = path;
    engine
        .promise(&convoy, "work", "coder", &submit, flotilla_resources::convoy::engine::Actor::Crew)
        .await
        .expect("the coder submits its PR");
    // The branch scan links the change request as what the convoy produces: a convoy's subjects
    // are the engine's links, and the owner's demand follows them (#3108).
    let produced = flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: IssueSource { service: "github.com".into(), scope: "org/shared".into() },
        id: "330".into(),
    };
    engine
        .link(&convoy, &produced, flotilla_resources::SubjectDiscoverySource::Branch, "branch-scan")
        .await
        .expect("the branch scan links it");
    launch
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
    let mesh = spawn_in_memory_request_mesh(vec![owner.clone(), requester.clone()]).await.unwrap();
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
            daemon.resource_backend(),
            daemon.node_id().to_string(),
            source,
            ChangeRequestRefreshCadence::default(),
        );
        let records = flotilla_provisioning::engine::observations::EngineObservations::open(
            daemon.decided_store(),
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
    let convoys = requester.resource_backend().using::<Convoy>("flotilla");
    let producer = produce(&requester, "producer").await;
    requester_observer.demand(uuid::Uuid::new_v4(), subject.clone(), None).await.unwrap();
    assert!(
        !change_request_homes(&requester, &name).await.contains(&requester.node_id().to_string()),
        "a requester that does not observe the source writes no record of its own"
    );
    if late_watcher {
        eventually(Duration::from_secs(5), Duration::from_millis(10), "convoy replicated before observer startup", || async {
            owner.resource_backend().including_replicas::<Convoy>("flotilla").get(&producer).await.is_ok()
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
    // The observer is the record's single writer (#3108): the requester's writes through the
    // old interface are refused, and every home still reads the observer's facts.
    let requester_records = requester.resource_backend().using::<ChangeRequest>("flotilla");
    let refused = requester_records.create(&InputMeta::builder().name(name.clone()).build(), &record.spec).await;
    assert!(
        refused.as_ref().is_err_and(|error| error.to_string().contains("written only by the observer of github.com org/shared")),
        "a requester's create is refused: {refused:?}"
    );
    // The requester holds no record of its own to write a status to.
    let held =
        requester.resource_backend().including_replicas::<ChangeRequest>("flotilla").get(&name).await.expect("the observer's record");
    let mut forged = held.object.status.clone().expect("the observer's status");
    forged.state = Observation::known(ObservedChangeRequestState::Closed, Utc::now());
    let refused = requester_records.update_status(&name, &held.object.metadata.resource_version, &forged).await;
    assert!(refused.is_err(), "a requester's status write is refused: {refused:?}");
    assert_eq!(change_request_homes(&requester, &name).await, vec![owner.node_id().to_string()]);
    assert_eq!(
        requester
            .resource_backend()
            .including_replicas::<ChangeRequest>("flotilla")
            .get(&name)
            .await
            .expect("the observer's record")
            .object
            .status
            .and_then(|status| status.state.value),
        Some(ObservedChangeRequestState::Open),
        "the observer's facts win"
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
    // The producer's convoy reconciler observes the merged record and keeps the promise on the
    // engine, one edge a pass.
    let reconciler = flotilla_crew::convoy::ConvoyReconciler::new(
        requester.lifecycle_reads(),
        requester.convoy_engine_path(),
        requester.crew_message_path(),
    )
    .with_change_requests(requester.resource_backend().including_replicas::<ChangeRequest>("flotilla"), Duration::from_secs(180));
    eventually(Duration::from_secs(5), Duration::from_millis(10), "merged observation keeps promise", || async {
        use flotilla_store::controller::Reconciler;
        let current = convoys.get(&producer).await.expect("producer");
        reconciler.prepare(&current).await.expect("observe the producer's promise");
        let kept = convoys.get(&producer).await.expect("producer");
        kept.status.expect("producer status").promises["work"]["coder"].iter().any(|promise| promise.state == PromiseState::Kept)
    })
    .await;
    // Duplicate convoy consumers share one observer demand. Ending one must preserve the other;
    // ending the last releases the standing demand. A convoy ends on the engine, never deleted (#3108).
    let second = produce(&requester, "second-producer").await;
    eventually(Duration::from_secs(5), Duration::from_millis(10), "second consumer replicated", || async {
        owner.resource_backend().including_replicas::<Convoy>("flotilla").get(&second).await.is_ok_and(|copy| copy.object.status.is_some())
    })
    .await;
    let end = |launch: String| {
        let (convoys, path) = (convoys.clone(), requester.convoy_engine_path());
        async move {
            let convoy = convoys.get(&launch).await.expect("consumer");
            path.abandon(&convoy, "operator", "consumer removed").await.expect("end the consumer");
        }
    };
    end(producer.clone()).await;
    let ended = |launch: String| {
        let owner = Arc::clone(&owner);
        async move {
            owner
                .resource_backend()
                .including_replicas::<Convoy>("flotilla")
                .get(&launch)
                .await
                .is_ok_and(|copy| copy.object.status.is_some_and(|status| status.phase == flotilla_resources::ConvoyPhase::Abandoned))
        }
    };
    eventually(Duration::from_secs(5), Duration::from_millis(10), "first consumer's end replicated", || ended(producer.clone())).await;
    assert_eq!(owner_observer.demanded_owned().await.expect("remaining owner demand"), vec![subject]);
    // An abandoned consumer's submitted PR is still observed until its promise resolves (#3108
    // ruling 1): the producer's ended pass keeps it on the merged observation, and that releases
    // the last demand.
    end(second.clone()).await;
    eventually(Duration::from_secs(5), Duration::from_millis(10), "last consumer releases demand", || async {
        use flotilla_store::controller::Reconciler;
        let current = convoys.get(&second).await.expect("second producer");
        reconciler.prepare(&current).await.expect("the ended pass observes the second producer's promise");
        owner_observer.demanded_owned().await.is_ok_and(|subjects| subjects.is_empty())
    })
    .await;
    drop(owner_demand_task.expect("owner watcher"));
    assert_eq!(forbidden.0.load(std::sync::atomic::Ordering::SeqCst), 0, "nonowner never reads the forge");
    drop(mesh);
}
