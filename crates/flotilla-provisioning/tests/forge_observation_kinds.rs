//! The forge observations' stored shape and import: deterministic DDL and a readable view, checked
//! in for the owner's schema look-over, the plans of their statements, and the import of every
//! observation the previous generation stored, at its source's observer alone.

mod schema_support;

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use flotilla_engine_store::Import;
use flotilla_protocol::{issue_query::IssueQuery, IssueSource};
use flotilla_provisioning::{
    engine::{
        forge_observation_import::{import_change_requests, import_forge_reads, import_issues, Demands, ObservationCopy},
        import::ImportReport,
    },
    stored::forge_observation::{self as stored, ChangeRequestRecord, ForgeReadRecord, IssueRecord, ReadValue},
};
use flotilla_resources::{
    ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, ChangeRequestSubjectHistory, ForgeRead,
    ForgeReadRequest, ForgeReadSpec, ForgeReadStatus, Issue, IssueSpec, IssueStatus, ObjectMeta, Observation, ObservedChangeRequestState,
    ObservedChecks, ObservedIssueState, ObservedMergeability, ObservedReviewDecision, Resource, ResourceObject,
};
use flotilla_schema::{render_view, Kind, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use hegel::{generators as gs, generators::Generator};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): the forge observations render deterministic DDL, checked in
// so the owner's look-over and every later change show as a diff.
#[test]
fn forge_observation_ddl_matches_the_snapshot() {
    check("forge_observation.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of the forge observations' tables.
#[test]
fn forge_observation_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Forge observation store schema view\n\nKind tables only; the shared tables are in `forge_observation.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("forge_observation_schema_view.md", &view);
}

// Behaviour (#3320 production shape): every statement over the forge observations' tables searches
// a key or a declared index, never a scan.
#[test]
fn forge_observation_statements_search_by_key() {
    let store =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", stored::registry().expect("registry")).expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let failures: Vec<_> = statements
        .iter()
        .map(|(label, sql)| (label, plan(&conn, sql)))
        .filter(|(label, rows)| schema_support::unkeyed(label, rows))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

const SERVICE: &str = "github.com";
const SCOPE: &str = "flotilla-org/flotilla";

fn epoch(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("time")
}

fn object<R: Resource>(name: &str, spec: R::Spec, status: Option<R::Status>) -> ResourceObject<R> {
    let metadata = ObjectMeta {
        name: name.into(),
        namespace: "flotilla".into(),
        resource_version: "1".into(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
        creation_timestamp: epoch(1_700_000_000),
        merge: None,
    };
    ResourceObject { metadata, spec, status }
}

fn copy<R: Resource>(object: ResourceObject<R>, home: &str) -> ObservationCopy<R> {
    ObservationCopy { object, home: home.into() }
}

fn known<T>(value: Option<T>, at: DateTime<Utc>) -> Observation<T> {
    Observation { value, observed_at: at }
}

fn cr_spec(scope: &str, number: u64, authority: &str) -> ChangeRequestSpec {
    ChangeRequestSpec {
        service: SERVICE.into(),
        scope: scope.into(),
        number,
        observing_authority: authority.into(),
        subject_of: Vec::new(),
    }
}

fn cr_status(state: ObservedChangeRequestState, at: DateTime<Utc>) -> ChangeRequestStatus {
    ChangeRequestStatus {
        title: known(Some("Layer 5".into()), at),
        author: known(None, at),
        review_decision: known(Some(ObservedReviewDecision::Required), at),
        review_requested_from_owner: known(Some(false), at),
        state: known(Some(state), at),
        head_sha: known(Some("abc123".into()), at),
        checks: known(Some(ObservedChecks::Pending), at),
        review: ChangeRequestReviewObservation { actionable_at_head: known(None, at) },
        mergeable: known(Some(ObservedMergeability::Mergeable), at),
    }
}

fn issue_spec(number: u64) -> IssueSpec {
    IssueSpec { service: SERVICE.into(), scope: SCOPE.into(), number, observing_authority: "alpha".into() }
}

fn issue_status(state: ObservedIssueState, at: DateTime<Utc>) -> IssueStatus {
    IssueStatus {
        title: known(Some("Remaining kinds".into()), at),
        assignees: known(Some(vec!["coder".into()]), at),
        state: known(Some(state), at),
        labels: known(Some(vec![]), at),
        updated_at: known(Some(at), at),
    }
}

fn source() -> IssueSource {
    IssueSource { service: "https://github.com".into(), scope: SCOPE.into() }
}

fn read_status(value: Option<serde_json::Value>, error: Option<&str>, at: DateTime<Utc>, authority: &str) -> ForgeReadStatus {
    ForgeReadStatus {
        authority: authority.into(),
        attempted_at: at,
        observed_at: Some(at),
        value,
        error: error.map(str::to_owned),
        retry_at: error.map(|_| at + Duration::seconds(60)),
    }
}

fn read(name: &str, request: ForgeReadRequest, demanded_at: DateTime<Utc>, status: Option<ForgeReadStatus>) -> ResourceObject<ForgeRead> {
    object(name, ForgeReadSpec { source: source(), request, demanded_at }, status)
}

fn observed_at_alpha(_: &IssueSource) -> Option<String> {
    Some("alpha".into())
}

fn import_at(home: &str) -> Import {
    Import::in_memory(home, stored::registry().expect("registry")).expect("store")
}

fn load<K: Kind>(target: &mut Import, name: &str, home: &str) -> Option<K> {
    let mut read = target.store().read().expect("read");
    let id = read.resolve(K::CODE, name, home).expect("resolve")?;
    read.load::<K>(id).expect("load").map(|loaded| loaded.value)
}

fn names<K: Kind>(target: &mut Import, home: &str) -> Vec<String> {
    let mut read = target.store().read().expect("read");
    let page = read.scan::<K>(home, None, 64).expect("scan");
    page.ids.into_iter().filter_map(|id| read.load::<K>(id).expect("load").map(|loaded| loaded.name)).collect()
}

fn maybe<T>(tc: &hegel::TestCase, value: T) -> Option<T> {
    tc.draw(gs::booleans()).then_some(value)
}

// Behaviour (ADR 0047): every change request the previous generation observed imports at its
// observer as its observed values, a value the forge never reported staying unknown.
#[hegel::test]
fn every_observed_change_request_imports_with_its_values(tc: hegel::TestCase) {
    let at = epoch(tc.draw(gs::integers::<i64>().min_value(0).max_value(4_000_000_000)));
    let state = tc.draw(
        gs::sampled_from(vec![
            ObservedChangeRequestState::Open,
            ObservedChangeRequestState::Draft,
            ObservedChangeRequestState::Merged,
            ObservedChangeRequestState::Closed,
        ])
        .print_as_debug(),
    );
    let decision = tc.draw(
        gs::sampled_from(vec![
            ObservedReviewDecision::Approved,
            ObservedReviewDecision::ChangesRequested,
            ObservedReviewDecision::Required,
            ObservedReviewDecision::None,
        ])
        .print_as_debug(),
    );
    let checks = tc.draw(gs::sampled_from(vec![ObservedChecks::Pass, ObservedChecks::Fail, ObservedChecks::Pending]).print_as_debug());
    let mergeable = tc.draw(gs::sampled_from(vec![ObservedMergeability::Mergeable, ObservedMergeability::Conflicting]).print_as_debug());
    let status = ChangeRequestStatus {
        title: known(maybe(&tc, tc.draw(gs::text().max_size(12))), at),
        author: known(maybe(&tc, tc.draw(gs::text().max_size(12))), at),
        review_decision: known(maybe(&tc, decision), at),
        review_requested_from_owner: known(maybe(&tc, tc.draw(gs::booleans())), at),
        state: known(maybe(&tc, state), at),
        head_sha: known(maybe(&tc, tc.draw(gs::text().max_size(40))), at),
        checks: known(maybe(&tc, checks), at),
        review: ChangeRequestReviewObservation { actionable_at_head: known(maybe(&tc, tc.draw(gs::booleans())), at) },
        mergeable: known(maybe(&tc, mergeable), at),
    };
    let number = tc.draw(gs::integers::<u64>().min_value(1).max_value(i64::MAX as u64));
    let old = object::<ChangeRequest>("cr", cr_spec(SCOPE, number, "alpha"), Some(status.clone()));
    let mut target = import_at("alpha");
    let mut report = ImportReport::default();
    import_change_requests(&[copy(old, "alpha")], &observed_at_alpha, &mut target, &mut report);
    assert!(report.refused.is_empty(), "{report:?}");
    let record: ChangeRequestRecord = load(&mut target, "cr", "alpha").expect("imported");
    assert_eq!((record.service.as_str(), record.scope.as_str(), record.number), (SERVICE, SCOPE, number));
    assert_eq!(record.title, status.title.value);
    assert_eq!(record.author, status.author.value);
    assert_eq!(record.review_decision.map(ObservedReviewDecision::from), status.review_decision.value);
    assert_eq!(record.review_requested_from_owner, status.review_requested_from_owner.value);
    assert_eq!(record.state.map(ObservedChangeRequestState::from), status.state.value);
    assert_eq!(record.head_sha, status.head_sha.value);
    assert_eq!(record.checks.map(ObservedChecks::from), status.checks.value);
    assert_eq!(record.actionable_at_head, status.review.actionable_at_head.value);
    assert_eq!(record.mergeable.map(ObservedMergeability::from), status.mergeable.value);
    assert_eq!(record.failing_since, None);
}

// Behaviour (ADR 0047): every issue the previous generation observed imports at its observer as its
// observed values, an unknown label or assignee list staying distinct from an empty one.
#[hegel::test]
fn every_observed_issue_imports_with_its_values(tc: hegel::TestCase) {
    let at = epoch(tc.draw(gs::integers::<i64>().min_value(0).max_value(4_000_000_000)));
    let updated = tc.draw(gs::integers::<i64>().min_value(-4_000_000_000).max_value(4_000_000_000));
    let names = |tc: &hegel::TestCase| tc.draw(gs::vecs(gs::text().max_size(8)).max_size(3));
    let status = IssueStatus {
        title: known(maybe(&tc, tc.draw(gs::text().max_size(12))), at),
        assignees: known(maybe(&tc, names(&tc)), at),
        state: known(
            maybe(&tc, tc.draw(gs::sampled_from(vec![ObservedIssueState::Open, ObservedIssueState::Closed]).print_as_debug())),
            at,
        ),
        labels: known(maybe(&tc, names(&tc)), at),
        updated_at: known(maybe(&tc, Utc.timestamp_micros(updated * 1_000_000 + 7).single().expect("time")), at),
    };
    let old = object::<Issue>("issue", issue_spec(7), Some(status.clone()));
    let mut target = import_at("alpha");
    let mut report = ImportReport::default();
    import_issues(&[copy(old, "alpha")], &observed_at_alpha, &mut target, &mut report);
    assert!(report.refused.is_empty(), "{report:?}");
    let record: IssueRecord = load(&mut target, "issue", "alpha").expect("imported");
    assert_eq!(record.title, status.title.value);
    assert_eq!(record.assignees, status.assignees.value);
    assert_eq!(record.state.map(ObservedIssueState::from), status.state.value);
    assert_eq!(record.labels, status.labels.value);
    assert_eq!(record.updated_at, status.updated_at.value.as_ref().map(Timestamp::from_datetime));
    assert_eq!(record.failing_since, None);
}

fn json(tc: &hegel::TestCase) -> serde_json::Value {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(5)) {
        0 => serde_json::Value::Null,
        1 => serde_json::Value::String(tc.draw(gs::text().max_size(8))),
        2 => serde_json::Value::from(tc.draw(gs::integers::<i64>())),
        3 => serde_json::json!({"items": [{"id": tc.draw(gs::text().max_size(4)), "open": tc.draw(gs::booleans())}], "page": 1}),
        4 => serde_json::json!([tc.draw(gs::integers::<u64>()), null, "x"]),
        _ => serde_json::Value::Bool(tc.draw(gs::booleans())),
    }
}

// Behaviour (ADR 0047): a forge read's latest result imports as its value, error and retry time,
// a successful read that found nothing (null) staying distinct from one that never succeeded.
#[hegel::test]
fn every_forge_read_result_imports_with_its_value(tc: hegel::TestCase) {
    let now = epoch(1_800_000_000);
    let value = maybe(&tc, json(&tc));
    let error = if value.is_none() || tc.draw(gs::booleans()) { Some("rate limited") } else { None };
    let request = tc.draw(
        gs::sampled_from(vec![
            ForgeReadRequest::Board,
            ForgeReadRequest::Query {
                params: IssueQuery { search: Some("layer".into()), label: None, match_fields: BTreeMap::new() },
                page: 2,
                count: 30,
            },
            ForgeReadRequest::Branch { branch: "main".into() },
            ForgeReadRequest::MergedBranches { limit: 50 },
        ])
        .print_as_debug(),
    );
    let status = read_status(value.clone(), error, now - Duration::seconds(30), "alpha");
    let old = read("forge-read-1", request.clone(), now - Duration::seconds(60), Some(status));
    let mut target = import_at("alpha");
    let mut report = ImportReport::default();
    import_forge_reads(&[copy(old, "alpha")], &Demands::new(), &observed_at_alpha, now, &mut target, &mut report);
    assert!(report.refused.is_empty(), "{report:?}");
    let record: ForgeReadRecord = load(&mut target, "forge-read-1", "alpha").expect("imported");
    assert_eq!(record.request, request);
    assert_eq!(record.source(), source());
    assert_eq!(record.value, value.map(|value| ReadValue { value }));
    assert_eq!(record.error.as_deref(), error);
    assert_eq!(record.retry_at, error.map(|_| Timestamp::from_datetime(&(now + Duration::seconds(30)))));
}

// Behaviour (ADR 0047; #3824 layer 5, operator correction 1): among every home's copies of one
// subject, the observer imports the one readers pick today; deleted, never-observed and
// undeclared copies are dropped, counted where they are homed, and convoy history is dropped.
#[test]
fn the_observer_imports_the_copy_readers_pick_and_counts_the_rest() {
    let observer_of = |source: &IssueSource| (source.scope == SCOPE).then(|| "alpha".to_owned());
    let stale = object::<ChangeRequest>("cr-1", cr_spec(SCOPE, 1, "alpha"), Some(cr_status(ObservedChangeRequestState::Open, epoch(100))));
    let mut fresh =
        object::<ChangeRequest>("cr-1", cr_spec(SCOPE, 1, "beta"), Some(cr_status(ObservedChangeRequestState::Merged, epoch(200))));
    fresh.spec.subject_of.push(ChangeRequestSubjectHistory {
        namespace: "flotilla".into(),
        convoy: "demo".into(),
        origin: "beta".into(),
        relationship: flotilla_protocol::Relationship::Produces,
        role: "coder".into(),
        project: None,
        last_seen: epoch(150),
    });
    let mut deleted = stale.clone();
    deleted.metadata.name = "cr-2".into();
    deleted.spec.number = 2;
    deleted.metadata.deletion_timestamp = Some(epoch(300));
    let placeholder = object::<ChangeRequest>("cr-3", cr_spec(SCOPE, 3, "alpha"), None);
    let undeclared =
        object::<ChangeRequest>("cr-4", cr_spec("someone/else", 4, "alpha"), Some(cr_status(ObservedChangeRequestState::Open, epoch(100))));
    let copies = [copy(stale, "alpha"), copy(fresh, "beta"), copy(deleted, "alpha"), copy(placeholder, "alpha"), copy(undeclared, "alpha")];

    let mut alpha = import_at("alpha");
    let mut report = ImportReport::default();
    import_change_requests(&copies, &observer_of, &mut alpha, &mut report);
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(names::<ChangeRequestRecord>(&mut alpha, "alpha"), ["cr-1"]);
    let record: ChangeRequestRecord = load(&mut alpha, "cr-1", "alpha").expect("imported");
    assert_eq!(record.state, Some(stored::ChangeRequestState::Merged));
    let dropped: Vec<_> = report.dropped.iter().map(|((_, reason), count)| (*reason, *count)).collect();
    assert_eq!(dropped, [("convoy history", 1), ("deleted", 1), ("never observed", 1), ("no declared observer", 1)]);

    let mut beta = import_at("beta");
    let mut report = ImportReport::default();
    import_change_requests(&copies, &observer_of, &mut beta, &mut report);
    assert!(names::<ChangeRequestRecord>(&mut beta, "beta").is_empty());
    let dropped: Vec<_> = report.dropped.iter().map(|((_, reason), count)| (*reason, *count)).collect();
    assert_eq!(dropped, [("imported at its observer", 1)]);
}

// Behaviour (ADR 0047): a forge read imports its latest attempt; one nothing has demanded within
// the retention, an incremental one, and one that never succeeded or failed are dropped. A
// heartbeat's renewal counts as demand.
#[test]
fn forge_reads_import_their_latest_result_while_demanded() {
    let now = epoch(1_800_000_000);
    let hour = Duration::seconds(3600);
    let older = read(
        "forge-read-a",
        ForgeReadRequest::Board,
        now - hour * 3,
        Some(read_status(Some(serde_json::json!(1)), None, now - hour * 3, "beta")),
    );
    let newer = read(
        "forge-read-a",
        ForgeReadRequest::Board,
        now - hour * 3,
        Some(read_status(Some(serde_json::json!(2)), None, now - hour * 2, "alpha")),
    );
    let idle = read(
        "forge-read-b",
        ForgeReadRequest::Branch { branch: "x".into() },
        now - hour * 2,
        Some(read_status(None, Some("gone"), now - hour * 2, "alpha")),
    );
    let incremental = read(
        "forge-read-c",
        ForgeReadRequest::Changes { since: "c1".into(), count: 10 },
        now,
        Some(read_status(Some(serde_json::json!([])), None, now, "alpha")),
    );
    let unread = read("forge-read-d", ForgeReadRequest::Mission { id: "7".into() }, now, Some(read_status(None, None, now, "alpha")));
    let renewed = Demands::from([("forge-read-a".to_owned(), now - Duration::seconds(30))]);
    let copies = [copy(older, "beta"), copy(newer, "alpha"), copy(idle, "alpha"), copy(incremental, "alpha"), copy(unread, "alpha")];

    let mut alpha = import_at("alpha");
    let mut report = ImportReport::default();
    import_forge_reads(&copies, &renewed, &observed_at_alpha, now, &mut alpha, &mut report);
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(names::<ForgeReadRecord>(&mut alpha, "alpha"), ["forge-read-a"]);
    let record: ForgeReadRecord = load(&mut alpha, "forge-read-a", "alpha").expect("imported");
    assert_eq!(record.value, Some(ReadValue { value: serde_json::json!(2) }));
    let dropped: Vec<_> = report.dropped.iter().map(|((_, reason), count)| (*reason, *count)).collect();
    assert_eq!(dropped, [("idle", 1), ("incremental", 1), ("never read", 1)]);
}

// Behaviour (#3824 layer 5, operator correction 1): hosts holding different copies, each importing
// only at itself, together write every observed subject exactly once, at its observer.
#[hegel::test]
fn hosts_importing_alone_write_each_subject_once_at_its_observer(tc: hegel::TestCase) {
    let homes = ["alpha", "beta", "gamma"];
    let scopes = ["o/one", "o/two", "o/three"];
    let observers: BTreeMap<&str, Option<&str>> =
        scopes.iter().map(|scope| (*scope, tc.draw(gs::sampled_from(vec![None, Some("alpha"), Some("beta"), Some("gamma")])))).collect();
    let observer_of = |source: &IssueSource| observers.get(source.scope.as_str()).copied().flatten().map(str::to_owned);
    let mut copies = Vec::new();
    for index in 0..tc.draw(gs::integers::<usize>().min_value(0).max_value(12)) {
        let scope = tc.draw(gs::sampled_from(scopes.to_vec()));
        let number = tc.draw(gs::integers::<u64>().min_value(1).max_value(3));
        let home = tc.draw(gs::sampled_from(homes.to_vec()));
        let seen = epoch(tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000)));
        let status = tc.draw(gs::booleans()).then(|| cr_status(ObservedChangeRequestState::Open, seen));
        let mut old = object::<ChangeRequest>(&format!("cr-{scope}-{number}"), cr_spec(scope, number, home), status);
        old.metadata.namespace = if index % 2 == 0 { "flotilla".into() } else { "other".into() };
        copies.push(copy(old, home));
    }

    let mut written = BTreeMap::<String, Vec<&str>>::new();
    for home in homes {
        let mut target = import_at(home);
        let mut report = ImportReport::default();
        import_change_requests(&copies, &observer_of, &mut target, &mut report);
        assert!(report.refused.is_empty(), "{report:?}");
        for name in names::<ChangeRequestRecord>(&mut target, home) {
            written.entry(name).or_default().push(home);
        }
    }
    let mut expected = BTreeMap::<String, Vec<&str>>::new();
    for copy in copies.iter().filter(|copy| copy.object.status.is_some()) {
        if let Some(observer) = observers[copy.object.spec.scope.as_str()] {
            expected.insert(copy.object.metadata.name.clone(), vec![observer]);
        }
    }
    assert_eq!(written, expected);
}

// Behaviour (ADR 0047): an issue imports from its freshest copy, at its observer only.
#[test]
fn an_issue_imports_its_freshest_copy_at_its_observer() {
    let stale = object::<Issue>("issue-7", issue_spec(7), Some(issue_status(ObservedIssueState::Open, epoch(100))));
    let fresh = object::<Issue>("issue-7", issue_spec(7), Some(issue_status(ObservedIssueState::Closed, epoch(200))));
    let copies = [copy(fresh, "beta"), copy(stale, "alpha")];
    for home in ["alpha", "beta"] {
        let mut target = import_at(home);
        let mut report = ImportReport::default();
        import_issues(&copies, &observed_at_alpha, &mut target, &mut report);
        assert!(report.refused.is_empty(), "{report:?}");
        let record: Option<IssueRecord> = load(&mut target, "issue-7", home);
        assert_eq!(record.map(|record| record.state), (home == "alpha").then_some(Some(stored::IssueState::Closed)), "{home}");
    }
}
