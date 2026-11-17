//! ChangeRequest's and Issue's reads and writes through the store's typed paths: one contract, run
//! against the old store and against the engine's records installed behind `[primitives]`, and the
//! engine's own rules: a write that changes no value writes nothing, a failed read ages the values
//! from when it started, and another home's values are confirmed when that home was last heard.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_provisioning::engine::observations::EngineObservations;
use flotilla_resources::{
    address::record_name,
    engine_records::{KindRecords, ObservationFailures, Record},
    ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, Clock, InputMeta, Issue, IssueSpec, IssueStatus,
    Observation, ObservedChangeRequestState, ObservedChecks, ObservedIssueState, ObservedMergeability, ObservedReviewDecision,
};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{watch_resource_kind_including_replicas, ResourceBackend, StoreQuery};
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;

const NS: &str = "flotilla";

#[derive(Debug)]
struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    fn at(seconds: i64) -> Arc<Self> {
        Arc::new(Self(Mutex::new(epoch(seconds))))
    }

    fn set(&self, seconds: i64) {
        *self.0.lock().expect("clock") = epoch(seconds);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().expect("clock")
    }
}

fn epoch(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("time")
}

fn cr_spec(number: u64, authority: &str) -> ChangeRequestSpec {
    ChangeRequestSpec {
        service: "github.com".into(),
        scope: "flotilla-org/flotilla".into(),
        number,
        observing_authority: authority.into(),
        subject_of: Vec::new(),
    }
}

/// Change request 1's record name, the one its observer writes it under.
fn cr_1() -> String {
    record_name(&cr_spec(1, "alpha").address(&[]).expect("address")).expect("record name")
}

fn cr_status(state: ObservedChangeRequestState, checks: ObservedChecks, at: DateTime<Utc>) -> ChangeRequestStatus {
    let seen = |value| Observation { value, observed_at: at };
    ChangeRequestStatus {
        title: Observation { value: Some("Layer 5".into()), observed_at: at },
        author: Observation { value: Some("coder".into()), observed_at: at },
        review_decision: Observation { value: Some(ObservedReviewDecision::Required), observed_at: at },
        review_requested_from_owner: seen(Some(false)),
        state: Observation { value: Some(state), observed_at: at },
        head_sha: Observation { value: Some("abc".into()), observed_at: at },
        checks: Observation { value: Some(checks), observed_at: at },
        review: ChangeRequestReviewObservation { actionable_at_head: seen(None) },
        mergeable: Observation { value: Some(ObservedMergeability::Mergeable), observed_at: at },
    }
}

fn issue_status(state: ObservedIssueState, at: DateTime<Utc>) -> IssueStatus {
    IssueStatus {
        title: Observation { value: Some("Remaining kinds".into()), observed_at: at },
        assignees: Observation { value: Some(vec!["coder".into()]), observed_at: at },
        state: Observation { value: Some(state), observed_at: at },
        labels: Observation { value: Some(vec!["enhancement".into()]), observed_at: at },
        updated_at: Observation { value: Some(epoch(1_700_000_000)), observed_at: at },
    }
}

fn values(status: &ChangeRequestStatus) -> (Option<ObservedChangeRequestState>, Option<ObservedChecks>, Option<String>) {
    (status.state.value, status.checks.value, status.title.value.clone())
}

/// The contract. Each step states the behaviour it checks.
async fn contract(backend: &ResourceBackend, observer: &str) {
    let change_requests = backend.using::<ChangeRequest>(NS);
    let mut watch = watch_resource_kind_including_replicas(backend, NS, "ChangeRequest").await.expect("watch");

    // The observer creates its record of a change request and publishes what it observed; every read finds it.
    let created = change_requests.create(&InputMeta::builder().name(cr_1()).build(), &cr_spec(1, observer)).await.expect("create");
    let open = cr_status(ObservedChangeRequestState::Open, ObservedChecks::Pending, Utc::now());
    let published = change_requests.update_status(&cr_1(), &created.metadata.resource_version, &open).await.expect("publish");
    tokio::time::timeout(Duration::from_secs(10), watch.stream.next()).await.expect("a watch event").expect("open").expect("event");
    let read = backend.including_replicas::<ChangeRequest>(NS).get(&cr_1()).await.expect("read").object;
    assert_eq!(read.spec.observing_authority, observer);
    assert_eq!((read.spec.service.as_str(), read.spec.number), ("github.com", 1));
    assert_eq!(values(read.status.as_ref().expect("status")), values(&open));

    // A change request is found by its number, and a merged one by its state.
    let by_number =
        StoreQuery { fields: BTreeMap::from([("spec.number".into(), "1".into())]), include_replicas: true, ..Default::default() };
    assert_eq!(backend.query_declared::<ChangeRequest>(NS, &by_number).await.expect("query").items.len(), 1);
    let merged_query = StoreQuery {
        fields: BTreeMap::from([("status.state.value".into(), "merged".into())]),
        include_replicas: true,
        ..Default::default()
    };
    assert!(backend.query_declared::<ChangeRequest>(NS, &merged_query).await.expect("query").items.is_empty());
    let merged = cr_status(ObservedChangeRequestState::Merged, ObservedChecks::Pass, Utc::now());
    change_requests.update_status(&cr_1(), &published.metadata.resource_version, &merged).await.expect("publish merged");
    assert_eq!(backend.query_declared::<ChangeRequest>(NS, &merged_query).await.expect("query").items.len(), 1);

    // An issue's record likewise.
    let issues = backend.using::<Issue>(NS);
    let spec =
        IssueSpec { service: "github.com".into(), scope: "flotilla-org/flotilla".into(), number: 7, observing_authority: observer.into() };
    let created = issues.create(&InputMeta::builder().name("issue-7".into()).build(), &spec).await.expect("create issue");
    let status = issue_status(ObservedIssueState::Open, Utc::now());
    issues.update_status("issue-7", &created.metadata.resource_version, &status).await.expect("publish issue");
    let read = backend.including_replicas::<Issue>(NS).get("issue-7").await.expect("read issue").object;
    let read = read.status.expect("status");
    assert_eq!(
        (read.state.value, read.labels.value, read.updated_at.value),
        (status.state.value, status.labels.value, status.updated_at.value)
    );

    // When nothing demands it any more, the observer deletes it; it is gone from every read.
    change_requests.delete(&cr_1()).await.expect("delete");
    assert!(backend.including_replicas::<ChangeRequest>(NS).get(&cr_1()).await.is_err());
    assert!(change_requests.list().await.expect("list").items.is_empty());
}

#[tokio::test]
async fn the_old_store_keeps_the_observation_contract() {
    contract(&ResourceBackend::new(InMemoryBackend::default()), "alpha").await;
}

fn open(directory: &tempfile::TempDir, home: &str, clock: Arc<ManualClock>) -> (Arc<Service>, Arc<EngineObservations>) {
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::forge_observation::register(&mut kinds).expect("register");
    let service = Arc::new(Service::open(&directory.path().join(format!("{home}.sqlite")), home, kinds).expect("store"));
    let records = EngineObservations::open(Arc::clone(&service), clock as Arc<dyn Clock>).expect("open");
    (service, Arc::new(records))
}

#[tokio::test]
async fn the_engine_keeps_the_observation_contract() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (_, records) = open(&directory, "alpha", ManualClock::at(1_800_000_000));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(records);
    contract(&backend, "alpha").await;
    for kind in [flotilla_store::ResourceKind::of::<ChangeRequest>(), flotilla_store::ResourceKind::of::<Issue>()] {
        assert!(backend.store().list_raw(kind, NS).await.expect("old rows").items.is_empty());
    }
}

// Behaviour (#3108 keyed reads): readers get a change request by its subject's record name, so a
// write under any other name is refused, naming both, and nothing is written.
#[tokio::test]
async fn a_change_request_under_another_name_is_refused() {
    let directory = tempfile::tempdir().expect("temp dir");
    let (_, records) = open(&directory, "alpha", ManualClock::at(1_800_000_000));
    let records = cr_port(&records);
    let spec = cr_spec(1, "alpha");
    let refused = records.apply("cr-1", None, &BTreeMap::new(), &BTreeMap::new(), &spec).await;
    let error = String::from(refused.expect_err("a name that is not the record name is refused"));
    assert!(error.contains("cr-1") && error.contains(&cr_1()), "{error}");
    assert!(records.sources().await.expect("sources").is_empty(), "nothing is written");
    records.apply(&cr_1(), None, &BTreeMap::new(), &BTreeMap::new(), &spec).await.expect("the record name is written");
}

/// The observation records as the ChangeRequest port.
fn cr_port(records: &Arc<EngineObservations>) -> &dyn KindRecords<ChangeRequest> {
    &**records
}

fn status_of(object: &Record<ChangeRequest>) -> ChangeRequestStatus {
    object.status.clone().expect("status")
}

// Behaviour (observation ruling: written only when a value changes): a publish that changes no
// value writes nothing, whatever its observation times; a changed value writes.
#[tokio::test]
async fn a_publish_that_changes_no_value_writes_nothing() {
    let directory = tempfile::tempdir().expect("temp dir");
    let clock = ManualClock::at(1_800_000_000);
    let (_, records) = open(&directory, "alpha", clock.clone());
    let records = cr_port(&records);
    let spec = cr_spec(1, "alpha");
    let created = records.apply(&cr_1(), None, &BTreeMap::new(), &BTreeMap::new(), &spec).await.expect("create");
    let status = |checks, at| cr_status(ObservedChangeRequestState::Open, checks, epoch(at));
    let first = records.set_status(&cr_1(), created.version, &status(ObservedChecks::Pending, 10)).await.expect("publish");
    let again = records.set_status(&cr_1(), first.version, &status(ObservedChecks::Pending, 20)).await.expect("publish again");
    let reapplied = records.apply(&cr_1(), None, &BTreeMap::new(), &BTreeMap::new(), &spec).await.expect("ensure again");
    assert_eq!((again.version, reapplied.version), (first.version, first.version));
    let changed = records.set_status(&cr_1(), reapplied.version, &status(ObservedChecks::Pass, 30)).await.expect("publish change");
    assert!(changed.version > first.version);
    assert_eq!(status_of(&changed).checks.value, Some(ObservedChecks::Pass));
}

// Behaviour (age as a judgement): this home's values read as confirmed now; once its reads fail,
// they read as confirmed when the failure started, however long it lasts, until a read succeeds.
#[tokio::test]
async fn a_failing_observer_ages_its_values_from_the_first_failure() {
    let directory = tempfile::tempdir().expect("temp dir");
    let clock = ManualClock::at(1_800_000_000);
    let (_, observations) = open(&directory, "alpha", clock.clone());
    let records = cr_port(&observations);
    let created = records.apply(&cr_1(), None, &BTreeMap::new(), &BTreeMap::new(), &cr_spec(1, "alpha")).await.expect("create");
    let open = cr_status(ObservedChangeRequestState::Open, ObservedChecks::Pass, epoch(0));
    records.set_status(&cr_1(), created.version, &open).await.expect("publish");
    let read = || async { status_of(&records.get(&cr_1()).await.expect("get").expect("held")).state.observed_at };
    assert_eq!(read().await, epoch(1_800_000_000));

    clock.set(1_800_000_100);
    observations.failing("ChangeRequest", &cr_1()).await.expect("failing");
    clock.set(1_800_000_900);
    let version = records.get(&cr_1()).await.expect("get").expect("held").version;
    observations.failing("ChangeRequest", &cr_1()).await.expect("still failing");
    assert_eq!(records.get(&cr_1()).await.expect("get").expect("held").version, version, "failing is recorded once");
    assert_eq!(read().await, epoch(1_800_000_100));

    records.set_status(&cr_1(), version, &open).await.expect("a read succeeds again");
    assert_eq!(read().await, epoch(1_800_000_900));
    observations.failing("ChangeRequest", "absent").await.expect("an absent record is done");
}

/// One round of `from`'s stream applied at `to`, served at `at`; a reset round when `cursor` is none.
fn pump(from: &Service, from_home: &str, to: &Service, cursor: Option<u64>, at: DateTime<Utc>) -> u64 {
    let mut frames = Vec::new();
    let next = from
        .serve(cursor, Timestamp::from_datetime(&at), |frame| {
            frames.push(frame.to_vec());
            Ok(())
        })
        .expect("serve");
    to.with_store(|store| {
        flotilla_schema_sqlite::stream::Follower::new(store).receive(from_home, |sink| frames.iter().try_for_each(|frame| sink(frame)))
    })
    .expect("follow");
    next
}

// Behaviour (one observer writes, every home reads): another home's record reads here with its
// values confirmed when that home was last heard, moving with each round even when nothing
// changed; of two homes' copies of one subject, the more recently confirmed is the one read.
#[tokio::test]
async fn another_homes_values_are_confirmed_when_that_home_was_last_heard() {
    let directory = tempfile::tempdir().expect("temp dir");
    let clock = ManualClock::at(1_800_000_000);
    let (beta, beta_observations) = open(&directory, "beta", clock.clone());
    let (alpha, alpha_observations) = open(&directory, "alpha", clock.clone());
    let (beta_records, alpha_records) = (cr_port(&beta_observations), cr_port(&alpha_observations));
    let spec = cr_spec(1, "beta");
    let created = beta_records.apply(&cr_1(), None, &BTreeMap::new(), &BTreeMap::new(), &spec).await.expect("create");
    let merged = cr_status(ObservedChangeRequestState::Merged, ObservedChecks::Pass, epoch(0));
    beta_records.set_status(&cr_1(), created.version, &merged).await.expect("publish");
    let cursor = pump(&beta, "beta", &alpha, None, epoch(1_800_000_050));
    let read = || async { alpha_records.get(&cr_1()).await.expect("get").expect("held") };
    assert_eq!((read().await.home, status_of(&read().await).state.observed_at), ("beta".to_owned(), epoch(1_800_000_050)));
    pump(&beta, "beta", &alpha, Some(cursor), epoch(1_800_000_070));
    assert_eq!(status_of(&read().await).state.observed_at, epoch(1_800_000_070));

    // A former observer's own copy, failing since before beta was last heard, reads below beta's.
    alpha_records.apply(&cr_1(), None, &BTreeMap::new(), &BTreeMap::new(), &spec).await.expect("own copy");
    clock.set(1_800_000_060);
    alpha_observations.failing("ChangeRequest", &cr_1()).await.expect("failing");
    clock.set(1_800_000_100);
    assert_eq!(read().await.home, "beta");
    assert_eq!(alpha_records.sources().await.expect("sources").len(), 2);
    assert_eq!(alpha_records.list().await.expect("list").len(), 1);
}

/// Observes every source, or none.
#[derive(Debug)]
struct Observes(bool);

#[async_trait::async_trait]
impl flotilla_resources::engine_records::SourceObservers for Observes {
    async fn observes(&self, _: &str, _: &str) -> Result<bool, String> {
        Ok(self.0)
    }
}

// Behaviour (single writer, #3108): a home that does not observe a change request's source is
// refused every write of it through the store's typed paths, its record and its failures alike,
// so no copy of its own, however recently written, stands beside the observer's; every read here
// is the observer's facts, replicated from its home.
#[tokio::test]
async fn a_home_that_does_not_observe_the_source_writes_nothing_and_reads_the_observers_facts() {
    let directory = tempfile::tempdir().expect("temp dir");
    let clock = ManualClock::at(1_800_000_000);
    let (beta, beta_records) = open(&directory, "beta", clock.clone());
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::forge_observation::register(&mut kinds).expect("register");
    let alpha = Arc::new(Service::open(&directory.path().join("alpha.sqlite"), "alpha", kinds).expect("store"));
    let alpha_records = Arc::new(
        EngineObservations::open(Arc::clone(&alpha), clock.clone() as Arc<dyn Clock>).expect("open").observed_by(Arc::new(Observes(false))),
    );
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend.install_engine_records(alpha_records.clone());

    let beta_port = cr_port(&beta_records);
    let created =
        beta_port.apply(&cr_1(), None, &BTreeMap::new(), &BTreeMap::new(), &cr_spec(1, "beta")).await.expect("the observer's record");
    let merged = cr_status(ObservedChangeRequestState::Merged, ObservedChecks::Pass, epoch(0));
    beta_port.set_status(&cr_1(), created.version, &merged).await.expect("publish");
    pump(&beta, "beta", &alpha, None, epoch(1_800_000_050));

    let change_requests = backend.using::<ChangeRequest>(NS);
    let refused = change_requests.create(&InputMeta::builder().name(cr_1()).build(), &cr_spec(1, "alpha")).await;
    let error = refused.expect_err("a home that does not observe the source writes no record of it").to_string();
    assert!(error.contains("written only by the observer of github.com flotilla-org/flotilla"), "{error}");
    let branch = flotilla_resources::engine_records::BranchChangeRequest {
        name: cr_1(),
        home: "alpha".into(),
        service: "github.com".into(),
        scope: "flotilla-org/flotilla".into(),
        number: 1,
        branch: "feature".into(),
        state: Some(ObservedChangeRequestState::Open),
    };
    use flotilla_resources::engine_records::ChangeRequestBranches;
    assert!(alpha_records.found_on_branch(&branch).await.is_err(), "nor what a branch read found");

    let read = backend.including_replicas::<ChangeRequest>(NS).get(&cr_1()).await.expect("read").object;
    assert_eq!(read.spec.observing_authority, "beta");
    assert_eq!(values(read.status.as_ref().expect("status")), values(&merged));
    let selected = flotilla_resources::select_change_requests(
        backend.including_replicas::<ChangeRequest>(NS).list().await.expect("list").items.iter().map(|copy| &copy.object),
    );
    assert_eq!(selected.values().map(|record| record.spec.observing_authority.as_str()).collect::<Vec<_>>(), vec!["beta"]);
    assert_eq!(
        cr_port(&alpha_records).sources().await.expect("sources").iter().map(|copy| copy.home.as_str()).collect::<Vec<_>>(),
        vec!["beta"]
    );
}
