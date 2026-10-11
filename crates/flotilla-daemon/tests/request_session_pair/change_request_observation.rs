use flotilla_core::{
    change_request_observer::{ChangeRequestRefreshCadence, ChangeRequestRefresher},
    providers::change_request::observation::{ChangeRequestObservationSource, ChangeRequestRef},
};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{
    change_request_record_name,
    promises::{self, PromiseKind, PromiseOperation, PromiseSource, PromiseState, Submission},
    ChangeRequest, ChangeRequestLeafSubject, ChangeRequestSpec, ChangeRequestStatus, ConvoyStatus, IssueSourceBindingSpec, LeafSubject,
    Observation, ObservedChangeRequestState, ObservedChecks, Project, ProjectSpec,
};
use tokio_util::task::AbortOnDropHandle;

use super::*;

// External forge boundary: the owner sees complete observations; any nonowner
// read is a failure. Transport, replication, convoys and observers are real.
struct ForgeObservation(tokio::sync::Mutex<ObservedChangeRequestState>);
#[async_trait]
impl ChangeRequestObservationSource for ForgeObservation {
    async fn observe(&self, _: &ChangeRequestRef) -> Result<ChangeRequestStatus, ObservationError> {
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
    async fn observe(&self, _: &ChangeRequestRef) -> Result<ChangeRequestStatus, ObservationError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(ObservationError::Failed("nonowner contacted forge".into()))
    }
}

// #3030: remote convoy demand must reach the source owner, repair a foreign
// authority, and replicate state/checks/readiness that can keep a PR promise.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn remote_produced_change_request_is_owner_observed() {
    remote_change_request_scenario(ExistingRecord::Requester, false).await;
}

// A source owner must also reclaim an existing local shadow, even when that
// shadow is attributed to the producer and contains stale forge observations.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn source_owner_recovers_stale_misattributed_local_record() {
    remote_change_request_scenario(ExistingRecord::StaleOwner, true).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn new_remote_change_request_names_source_owner() {
    remote_change_request_scenario(ExistingRecord::Missing, false).await;
}

#[derive(Clone, Copy)]
enum ExistingRecord {
    Missing,
    Requester,
    StaleOwner,
}

// Generated schedules cover missing records, empty/stale shadows and demand arriving
// before/after watcher startup, through the same routed replication scenario.
#[hegel::test]
fn generated_remote_change_request_observation(tc: hegel::TestCase) {
    let existing = [ExistingRecord::Missing, ExistingRecord::Requester, ExistingRecord::StaleOwner]
        [tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
    let late_watcher = tc.draw(gs::booleans());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime")
        .block_on(remote_change_request_scenario(existing, late_watcher));
}

async fn remote_change_request_scenario(existing: ExistingRecord, late_watcher: bool) {
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
    let mesh = spawn_in_memory_request_mesh_with_replication_kinds(
        vec![owner.clone(), requester.clone()],
        Some(&[Project::API_PATHS.kind, Host::API_PATHS.kind, Convoy::API_PATHS.kind, ChangeRequest::API_PATHS.kind]),
    )
    .await
    .unwrap();
    eventually(Duration::from_secs(5), Duration::from_millis(10), "source owner replicated", || async {
        flotilla_core::forge_observation::source_owner(&requester.resource_backend(), "flotilla", &source).await
            == Ok(Some(owner.node_id().clone()))
    })
    .await;
    let subject =
        ChangeRequestRef { namespace: "flotilla".into(), service: source.service.clone(), scope: source.scope.clone(), number: 330 };
    let name = change_request_record_name(&source.service, &source.scope, 330);
    // Existing misattribution from the live failure: an empty nonowner record.
    if !matches!(existing, ExistingRecord::Missing) {
        requester
            .resource_backend()
            .using::<ChangeRequest>("flotilla")
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &ChangeRequestSpec::builder()
                    .service(source.service.clone())
                    .scope(source.scope.clone())
                    .number(330)
                    .observing_authority(requester.node_id().to_string())
                    .build(),
            )
            .await
            .unwrap();
    }
    let forge = Arc::new(ForgeObservation(tokio::sync::Mutex::new(ObservedChangeRequestState::Open)));
    let refresher = |daemon: &Arc<InProcessDaemon>, source: Arc<dyn ChangeRequestObservationSource>| {
        ChangeRequestRefresher::new(
            daemon.host_name().to_string(),
            daemon.resource_backend(),
            daemon.node_id().to_string(),
            source,
            ChangeRequestRefreshCadence::default(),
        )
    };
    if matches!(existing, ExistingRecord::StaleOwner) {
        let records = owner.resource_backend().using::<ChangeRequest>("flotilla");
        let created = records
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &ChangeRequestSpec::builder()
                    .service(source.service.clone())
                    .scope(source.scope.clone())
                    .number(330)
                    .observing_authority(requester.node_id().to_string())
                    .build(),
            )
            .await
            .expect("owner's foreign shadow");
        let mut stale = forge.observe(&subject).await.expect("fixture observation");
        stale.state = Observation::known(ObservedChangeRequestState::Draft, Utc::now() - chrono::Duration::hours(1));
        records.update_status(&name, &created.metadata.resource_version, &stale).await.expect("stale status");
    }
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
    if matches!(existing, ExistingRecord::Missing) {
        let record = requester.resource_backend().using::<ChangeRequest>("flotilla").get(&name).await.expect("new requester record");
        assert_eq!(record.spec.observing_authority, owner.node_id().to_string(), "new records name the source owner");
    }
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
        let Some(patch) = promises::next_observation(&status, "producer", &records, &BTreeMap::new(), Utc::now(), Duration::from_secs(180))
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
