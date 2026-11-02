use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_issue_tracker::{
    source_feed::{Fact, FeedChangeRequest, FeedIssue, FeedItem, FeedPass, FeedRequest, ListCursor, SourceCursor, SourceFeed},
    IssueProvider,
};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    Issue, IssueChangeset, IssueRef, IssueSource, NodeId, Relationship, Subject, SubjectKind,
};
use flotilla_resources::{
    forge_observation::{
        ChangeRequestRecord, ChangeRequestState, Checks, Held, IssueRecord, IssueState, IssueSubject, ObservedKind, SourceRecords,
    },
    Clock, Convoy, ConvoySpec, DeclaredSubject, InputMeta, IssueSourceBindingSpec, ObservedChangeRequestState, ObservedChecks,
    ObservedIssueState, Project, ProjectSpec,
};
use flotilla_schema::Timestamp;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_testkit::VirtualClock;

use super::*;
use crate::sources::IssueQueryPort;

const NS: &str = "flotilla";

fn feed_source() -> IssueSource {
    IssueSource { service: "https://github.com".into(), scope: "org/shared".into() }
}

fn link(scope: &str, number: u64) -> IssueRef {
    IssueRef { source: IssueSource { service: "https://github.com".into(), scope: scope.into() }, id: number.to_string() }
}

fn stored(number: u64) -> IssueSubject {
    IssueSubject { service: "github.com".into(), scope: "org/shared".into(), number }
}

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000 + seconds, 0).expect("time")
}

fn issue(number: u64, state: ObservedIssueState) -> FeedIssue {
    FeedIssue {
        number,
        title: format!("Issue {number}"),
        state,
        body: Some("The contract.".into()),
        labels: vec!["ready".into()],
        assignees: vec![],
        updated_at: at(0),
        issue_type: Fact::Read(Some("Task".into())),
        parent: Fact::Read(None),
        blocked_by: Fact::Read(vec![]),
        fields: Fact::Read(BTreeMap::from([("Priority".into(), "High".into())])),
    }
}

fn pull(number: u64, checks: ObservedChecks) -> FeedChangeRequest {
    FeedChangeRequest {
        number,
        title: format!("Change {number}"),
        author: Some("coder".into()),
        state: ObservedChangeRequestState::Open,
        head_sha: Some("abc".into()),
        head_branch: Some("feature".into()),
        base_branch: Some("main".into()),
        checks: Fact::Read(Some(checks)),
        review_decision: Fact::Read(None),
        mergeable: Fact::Unread,
        closes: Fact::Read(vec![link("org/shared", 1)]),
    }
}

fn cursor(seconds: i64) -> SourceCursor {
    let list = ListCursor { at: at(seconds), seen: BTreeSet::new() };
    SourceCursor { issues: Some(list.clone()), pulls: Some(list) }
}

fn feed(issues: Vec<FeedIssue>, pulls: Vec<FeedChangeRequest>) -> FeedPass {
    FeedPass { cursor: cursor(0), issues, pulls, unread: vec![], gone: vec![] }
}

/// This home's records: written only when a value changes, each write counted as one log entry.
struct Records {
    clock: Arc<dyn Clock>,
    issues: Mutex<BTreeMap<String, (IssueRecord, Timestamp)>>,
    pulls: Mutex<BTreeMap<String, (ChangeRequestRecord, Timestamp)>>,
    entries: Mutex<usize>,
    whole: Mutex<BTreeSet<(String, String)>>,
}

impl Records {
    fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self { clock, issues: Mutex::default(), pulls: Mutex::default(), entries: Mutex::default(), whole: Mutex::default() })
    }

    fn entries(&self) -> usize {
        *self.entries.lock().expect("entries")
    }

    fn issue(&self, number: u64) -> Option<IssueRecord> {
        self.issues.lock().expect("issues").values().map(|(record, _)| record.clone()).find(|record| record.number == number)
    }

    fn pull(&self, number: u64) -> Option<ChangeRequestRecord> {
        self.pulls.lock().expect("pulls").values().map(|(record, _)| record.clone()).find(|record| record.number == number)
    }

    fn write<K: PartialEq + Clone>(&self, map: &Mutex<BTreeMap<String, (K, Timestamp)>>, name: &str, record: &K) -> bool {
        let mut map = map.lock().expect("records");
        if map.get(name).is_some_and(|(held, _)| held == record) {
            return false;
        }
        map.insert(name.to_owned(), (record.clone(), Timestamp::from_datetime(&self.clock.now())));
        *self.entries.lock().expect("entries") += 1;
        true
    }
}

#[async_trait]
impl SourceRecords for Records {
    fn observe_whole(&self, service: &str, scope: &str) {
        self.whole.lock().expect("whole").insert((service.into(), scope.into()));
    }

    fn release_whole(&self, service: &str, scope: &str) {
        self.whole.lock().expect("whole").remove(&(service.into(), scope.into()));
    }

    async fn issues(&self, service: &str, scope: &str) -> Result<Vec<Held<IssueRecord>>, String> {
        let issues = self.issues.lock().expect("issues");
        let held = issues.iter().filter(|(_, (record, _))| record.service == service && record.scope == scope);
        Ok(held.map(|(name, (record, _))| Held { name: name.clone(), record: record.clone() }).collect())
    }

    async fn change_requests(&self, service: &str, scope: &str) -> Result<Vec<Held<ChangeRequestRecord>>, String> {
        let pulls = self.pulls.lock().expect("pulls");
        let held = pulls.iter().filter(|(_, (record, _))| record.service == service && record.scope == scope);
        Ok(held.map(|(name, (record, _))| Held { name: name.clone(), record: record.clone() }).collect())
    }

    async fn write_issue(&self, name: &str, record: &IssueRecord) -> Result<bool, String> {
        Ok(self.write(&self.issues, name, record))
    }

    async fn write_change_request(&self, name: &str, record: &ChangeRequestRecord) -> Result<bool, String> {
        Ok(self.write(&self.pulls, name, record))
    }

    async fn last_written(&self, kind: ObservedKind, name: &str) -> Result<Option<Timestamp>, String> {
        Ok(match kind {
            ObservedKind::Issue => self.issues.lock().expect("issues").get(name).map(|(_, at)| *at),
            ObservedKind::ChangeRequest => self.pulls.lock().expect("pulls").get(name).map(|(_, at)| *at),
        })
    }

    async fn remove(&self, kind: ObservedKind, name: &str) -> Result<(), String> {
        let removed = match kind {
            ObservedKind::Issue => self.issues.lock().expect("issues").remove(name).is_some(),
            ObservedKind::ChangeRequest => self.pulls.lock().expect("pulls").remove(name).is_some(),
        };
        if removed {
            *self.entries.lock().expect("entries") += 1;
        }
        Ok(())
    }

    async fn referenced_by_open_issues(&self) -> Result<BTreeSet<IssueSubject>, String> {
        let issues = self.issues.lock().expect("issues");
        let open = issues.values().map(|(record, _)| record).filter(|record| record.state == Some(IssueState::Open));
        Ok(open
            .flat_map(|record| {
                let parent = match (&record.parent_service, &record.parent_scope, record.parent_number) {
                    (Some(service), Some(scope), Some(number)) => {
                        Some(IssueSubject { service: service.clone(), scope: scope.clone(), number })
                    }
                    _ => None,
                };
                record.blocked_by.iter().cloned().chain(parent)
            })
            .collect())
    }
}

#[derive(Default)]
struct States {
    saved: Mutex<BTreeMap<(String, String), SourceState>>,
    saves: Mutex<usize>,
    /// Whether saving fails.
    refuse: std::sync::atomic::AtomicBool,
}

impl SourceStates for States {
    fn load(&self, service: &str, scope: &str) -> Result<Option<SourceState>, String> {
        Ok(self.saved.lock().expect("states").get(&(service.into(), scope.into())).cloned())
    }

    fn save(&self, state: &SourceState) -> Result<(), String> {
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("disk full".into());
        }
        *self.saves.lock().expect("saves") += 1;
        self.saved.lock().expect("states").insert((state.service.clone(), state.scope.clone()), state.clone());
        Ok(())
    }

    fn remove(&self, service: &str, scope: &str) -> Result<(), String> {
        self.saved.lock().expect("states").remove(&(service.into(), scope.into()));
        Ok(())
    }

    fn sources(&self) -> Result<Vec<(String, String)>, String> {
        Ok(self.saved.lock().expect("states").keys().cloned().collect())
    }
}

/// The forge's feed for the test source: each pass answers with the next scripted result, or
/// reports nothing once the script is spent, and records what it was asked for.
#[derive(Default)]
struct Feed {
    script: Mutex<VecDeque<Result<FeedPass, String>>>,
    requests: Mutex<Vec<FeedRequest>>,
}

impl Feed {
    fn next(&self, pass: Result<FeedPass, String>) {
        self.script.lock().expect("script").push_back(pass);
    }

    fn last_request(&self) -> FeedRequest {
        self.requests.lock().expect("requests").last().cloned().expect("a pass was made")
    }

    fn requests(&self) -> Vec<FeedRequest> {
        self.requests.lock().expect("requests").clone()
    }

    /// Each pass's start, in seconds after `at(0)`.
    fn starts(&self) -> Vec<i64> {
        self.requests().iter().map(|request| (request.started - at(0)).num_seconds()).collect()
    }
}

#[async_trait]
impl SourceFeed for Feed {
    async fn pass(&self, source: &IssueSource, request: &FeedRequest) -> Result<FeedPass, String> {
        assert_eq!(source, &feed_source());
        self.requests.lock().expect("requests").push(request.clone());
        self.script.lock().expect("script").pop_front().unwrap_or_else(|| Ok(feed(vec![], vec![])))
    }
}

/// The observer's clock in tests of `run`: `at(0)` plus the paused tokio time elapsed.
struct TokioClock {
    start: tokio::time::Instant,
}

impl Clock for TokioClock {
    fn now(&self) -> DateTime<Utc> {
        at(0) + chrono::Duration::from_std(self.start.elapsed()).expect("elapsed")
    }
}

struct Provider(Arc<Feed>);

#[async_trait]
impl IssueProvider for Provider {
    fn supports(&self, source: &IssueSource) -> bool {
        source == &feed_source()
    }

    fn source_feed(&self) -> Option<&dyn SourceFeed> {
        Some(self.0.as_ref())
    }

    async fn query(&self, _: &IssueSource, _: &IssueQuery, _: u32, _: usize) -> Result<IssueResultPage, String> {
        Err("whole-source passes do not query".into())
    }

    async fn fetch_by_id(&self, _: &IssueRef) -> Result<Issue, String> {
        Err("whole-source passes do not fetch".into())
    }

    async fn list_changed_since(&self, _: &IssueSource, _: &str, _: usize) -> Result<IssueChangeset, String> {
        Err("whole-source passes do not list changes".into())
    }

    async fn open_in_browser(&self, _: &IssueRef) -> Result<(), String> {
        Err("no browser".into())
    }
}

struct Queries {
    feed: Arc<Feed>,
    /// Whether the local provider lookup fails.
    refuse: std::sync::atomic::AtomicBool,
}

impl Queries {
    fn new(feed: &Arc<Feed>) -> Arc<Self> {
        Arc::new(Self { feed: Arc::clone(feed), refuse: Default::default() })
    }
}

#[async_trait]
impl IssueQueryPort for Queries {
    async fn provider_for_source(&self, source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
        assert_eq!(source, &feed_source());
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("credentials unreadable".into());
        }
        Ok(Arc::new(Provider(Arc::clone(&self.feed))))
    }
}

struct World {
    backend: ResourceBackend,
    clock: Arc<VirtualClock>,
    records: Arc<Records>,
    states: Arc<States>,
    feed: Arc<Feed>,
    queries: Arc<Queries>,
    observer: SourceObserver,
}

impl World {
    async fn new() -> Self {
        let clock = Arc::new(VirtualClock::new(at(0)));
        Self::with_clock(clock.clone(), clock).await
    }

    /// A world whose observer's clock follows the paused tokio time, for tests of `run`.
    async fn running() -> Self {
        Self::with_clock(Arc::new(VirtualClock::new(at(0))), Arc::new(TokioClock { start: tokio::time::Instant::now() })).await
    }

    async fn with_clock(clock: Arc<VirtualClock>, observer_clock: Arc<dyn Clock>) -> Self {
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("owner"));
        let spec = ProjectSpec::builder()
            .display_name("Shared".into())
            .issue_source_bindings(vec![IssueSourceBindingSpec::builder().source(feed_source()).alias("shared".into()).build()])
            .build();
        backend.using::<Project>(NS).create(&InputMeta::builder().name("shared".into()).build(), &spec).await.expect("project");
        let records = Records::new(Arc::clone(&observer_clock));
        let states = Arc::new(States::default());
        let feed = Arc::new(Feed::default());
        let queries = Queries::new(&feed);
        let observer = SourceObserver::new(
            SourceObserverParts::builder()
                .backend(backend.clone())
                .namespace(NS.into())
                .queries(queries.clone())
                .records(records.clone())
                .states(states.clone())
                .clock(observer_clock)
                .build(),
        );
        Self { backend, clock, records, states, feed, queries, observer }
    }

    async fn source(&self) -> Observed {
        let owned = self.observer.owned().await.expect("owned");
        let [source] = owned.as_slice() else { panic!("one owned source: {owned:?}") };
        source.clone()
    }

    async fn pass(&self, pass: Result<FeedPass, String>) -> Result<(), PassError> {
        self.feed.next(pass);
        self.pass_asking(Asks { pending: true, ..Asks::default() }).await
    }

    async fn pass_asking(&self, asks: Asks) -> Result<(), PassError> {
        let source = self.source().await;
        self.observer.pass(&source, &asks).await.map(|_| ())
    }

    async fn address(&self, kind: Kind, number: u64) -> Address {
        self.source().await.repository.child(kind, &number.to_string()).expect("address")
    }

    fn run(&self) -> tokio::task::JoinHandle<()> {
        let observer = self.observer.clone();
        tokio::spawn(async move { observer.run().await })
    }

    async fn convoy_refers_to(&self, name: &str, kind: SubjectKind, number: u64) {
        let subject = DeclaredSubject {
            subject: Subject {
                kind,
                source: IssueSource { service: "github.com".into(), scope: "org/shared".into() },
                id: number.to_string(),
            },
            relationship: Relationship::Produces,
            issue: None,
            change_request: None,
        };
        let convoy = ConvoySpec::builder().workflow_ref("dev".into()).role("coder".into()).subjects(vec![subject]).build();
        self.backend.using::<Convoy>(NS).create(&InputMeta::builder().name(name.into()).build(), &convoy).await.expect("convoy");
    }
}

#[tokio::test]
async fn the_preferred_observer_observes_its_sources_whole() {
    let world = World::new().await;
    let source = world.source().await;
    assert_eq!((source.service.as_str(), source.scope.as_str()), ("github.com", "org/shared"));
    assert!(source.relayed, "the relay hints GitHub change requests");
    let elsewhere = SourceObserver::new(
        SourceObserverParts::builder()
            .backend(ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("other")))
            .namespace(NS.into())
            .queries(Queries::new(&world.feed))
            .records(world.records.clone())
            .states(world.states.clone())
            .clock(world.clock.clone())
            .build(),
    );
    assert!(elsewhere.owned().await.expect("owned").is_empty(), "a host that declares nothing observes nothing");
    world.pass(Ok(feed(vec![], vec![]))).await.expect("pass");
    assert!(world.records.whole.lock().expect("whole").contains(&("github.com".into(), "org/shared".into())));
}

#[tokio::test]
async fn a_pass_that_sees_nothing_new_writes_nothing() {
    let world = World::new().await;
    let items = || feed(vec![issue(1, ObservedIssueState::Open)], vec![pull(5, ObservedChecks::Pass)]);
    world.pass(Ok(items())).await.expect("first pass");
    assert_eq!(world.records.entries(), 2, "one record per issue and per change request");
    let saves = *world.states.saves.lock().expect("saves");
    world.pass(Ok(items())).await.expect("same values again");
    world.pass(Ok(feed(vec![], vec![]))).await.expect("nothing reported");
    assert_eq!(world.records.entries(), 2, "no value changed, so nothing is written");
    assert_eq!(*world.states.saves.lock().expect("saves"), saves, "an unmoved cursor is not saved again");

    let mut retitled = issue(1, ObservedIssueState::Open);
    retitled.title = "Retitled".into();
    world.pass(Ok(feed(vec![retitled], vec![]))).await.expect("a change");
    assert_eq!(world.records.entries(), 3);
    assert_eq!(world.records.issue(1).expect("held").title.as_deref(), Some("Retitled"));
}

#[tokio::test]
async fn failing_reads_mark_records_once_and_recovery_clears_them_once() {
    let world = World::new().await;
    world.pass(Ok(feed(vec![issue(1, ObservedIssueState::Open)], vec![pull(5, ObservedChecks::Pass)]))).await.expect("pass");
    world.clock.advance(chrono::Duration::seconds(30));
    assert!(world.pass(Err("forge down".into())).await.is_err());
    let since = Some(Timestamp::from_datetime(&at(30)));
    assert_eq!((world.records.issue(1).expect("issue").failing_since, world.records.pull(5).expect("pull").failing_since), (since, since));
    assert_eq!(world.records.entries(), 4);
    world.clock.advance(chrono::Duration::seconds(30));
    assert!(world.pass(Err("forge still down".into())).await.is_err());
    assert_eq!(world.records.entries(), 4, "a failure already recorded is not written again");
    assert_eq!(world.records.issue(1).expect("issue").failing_since, since, "values age from the first failure");

    world.pass(Ok(feed(vec![], vec![]))).await.expect("recovered");
    assert_eq!(world.records.entries(), 6);
    assert_eq!((world.records.issue(1).expect("issue").failing_since, world.records.pull(5).expect("pull").failing_since), (None, None));
    world.pass(Ok(feed(vec![], vec![]))).await.expect("healthy");
    assert_eq!(world.records.entries(), 6, "recovery is cleared once");
}

#[tokio::test]
async fn an_unread_item_is_marked_failing_and_read_again() {
    let world = World::new().await;
    world.pass(Ok(feed(vec![issue(1, ObservedIssueState::Open)], vec![]))).await.expect("pass");
    let unread = FeedPass { unread: vec![(FeedItem::Issue, 1, "not found".into())], ..feed(vec![], vec![]) };
    world.pass(Ok(unread)).await.expect("pass with an unread item");
    assert!(world.records.issue(1).expect("issue").failing_since.is_some());
    world.pass(Ok(feed(vec![issue(1, ObservedIssueState::Open)], vec![]))).await.expect("read again");
    assert!(world.feed.last_request().issues.contains(&1), "the unread item is asked for again");
    assert_eq!(world.records.issue(1).expect("issue").failing_since, None);
    world.pass(Ok(feed(vec![], vec![]))).await.expect("pass");
    assert!(world.feed.last_request().issues.is_empty());
}

#[tokio::test]
async fn passes_ask_for_referenced_items_not_held_and_pending_checks() {
    let world = World::new().await;
    let mut blocked = issue(1, ObservedIssueState::Open);
    blocked.blocked_by = Fact::Read(vec![link("org/shared", 2), link("org/other", 9)]);
    blocked.parent = Fact::Read(Some(link("org/shared", 3)));
    world.pass(Ok(feed(vec![blocked, issue(2, ObservedIssueState::Closed)], vec![pull(5, ObservedChecks::Pending)]))).await.expect("pass");
    world.convoy_refers_to("worker", SubjectKind::Issue, 4).await;
    world.convoy_refers_to("lander", SubjectKind::ChangeRequest, 6).await;
    world.pass(Ok(feed(vec![], vec![]))).await.expect("pass");
    let request = world.feed.last_request();
    assert_eq!(request.issues, BTreeSet::from([3, 4]), "an unheld parent and a convoy's issue; held and other sources' links are not");
    assert_eq!(request.pulls, BTreeSet::from([5, 6]), "a change request with pending checks and a convoy's change request");
    assert_eq!(request.cursor, cursor(0));
}

#[tokio::test]
async fn the_writer_keeps_what_the_forge_did_not_read() {
    let forges = [];
    let held = IssueRecord {
        issue_type: Some("Bug".into()),
        parent_service: Some("github.com".into()),
        parent_scope: Some("org/shared".into()),
        parent_number: Some(3),
        parent_observed: true,
        fields: Some(BTreeMap::from([("Weight".into(), "3".into())])),
        ..issue_record(None, "github.com", "org/shared", &issue(1, ObservedIssueState::Open), &forges)
    };
    let unread = FeedIssue { issue_type: Fact::Unread, parent: Fact::Unread, fields: Fact::Unread, ..issue(1, ObservedIssueState::Closed) };
    let record = issue_record(Some(&held), "github.com", "org/shared", &unread, &forges);
    assert_eq!(record.state, Some(IssueState::Closed));
    assert_eq!((record.issue_type, record.parent_number, record.fields), (held.issue_type, held.parent_number, held.fields));

    let mut undeclared = issue(1, ObservedIssueState::Open);
    undeclared.blocked_by = Fact::Read(vec![
        link("org/shared", 2),
        IssueRef { source: IssueSource { service: "https://code.example".into(), scope: "a/b".into() }, id: "1".into() },
    ]);
    let record = issue_record(None, "github.com", "org/shared", &undeclared, &forges);
    assert_eq!((record.blocked_by, record.blocked_by_observed), (vec![], false), "a link that cannot be stored leaves the list unobserved");

    let bound = ChangeRequestRecord {
        actionable_at_head: Some(true),
        review_requested_from_owner: Some(false),
        mergeable: Some(flotilla_resources::forge_observation::Mergeability::Conflicting),
        ..change_request_record(None, "github.com", "org/shared", &pull(5, ObservedChecks::Pass), &forges)
    };
    let record = change_request_record(Some(&bound), "github.com", "org/shared", &pull(5, ObservedChecks::Fail), &forges);
    assert_eq!(record.checks, Some(Checks::Fail));
    assert_eq!(
        (record.actionable_at_head, record.review_requested_from_owner, record.mergeable),
        (bound.actionable_at_head, bound.review_requested_from_owner, bound.mergeable)
    );
    assert_eq!((record.closes, record.closes_observed, record.state), (vec![stored(1)], true, Some(ChangeRequestState::Open)));
}

#[tokio::test]
async fn closed_items_nothing_refers_to_are_swept_thirty_days_after_closing() {
    let world = World::new().await;
    let mut open = issue(1, ObservedIssueState::Open);
    open.blocked_by = Fact::Read(vec![link("org/shared", 2)]);
    let mut merged = pull(6, ObservedChecks::Pass);
    merged.state = ObservedChangeRequestState::Merged;
    let closed = |number| issue(number, ObservedIssueState::Closed);
    world
        .pass(Ok(feed(vec![open, closed(2), closed(3), closed(4)], vec![merged.clone(), FeedChangeRequest { number: 7, ..merged }])))
        .await
        .expect("pass");
    world.convoy_refers_to("worker", SubjectKind::Issue, 4).await;
    world.convoy_refers_to("lander", SubjectKind::ChangeRequest, 7).await;
    let source = world.source().await;

    world.clock.advance(chrono::Duration::days(29));
    world.observer.sweep(&source).await.expect("sweep");
    assert!(world.records.issue(3).is_some() && world.records.pull(6).is_some(), "closed under thirty days ago");
    world.clock.advance(chrono::Duration::days(2));
    world.observer.sweep(&source).await.expect("sweep");
    assert!(world.records.issue(3).is_none() && world.records.pull(6).is_none(), "nothing refers to them");
    assert!(world.records.issue(2).is_some(), "an open issue is blocked by it");
    assert!(world.records.issue(4).is_some() && world.records.pull(7).is_some(), "a live convoy refers to them");
    assert!(world.records.issue(1).is_some(), "open items stay");
}

#[tokio::test]
async fn a_source_this_host_no_longer_observes_is_released() {
    let world = World::new().await;
    world.pass(Ok(feed(vec![issue(1, ObservedIssueState::Open)], vec![pull(5, ObservedChecks::Pass)]))).await.expect("pass");
    let owned = world.observer.owned().await.expect("owned");
    world.observer.release_unowned(&owned).await.expect("still owned");
    assert!(world.records.issue(1).is_some());
    world.observer.release_unowned(&[]).await.expect("unowned");
    world.clock.advance(chrono::Duration::minutes(14));
    world.observer.release_unowned(&[]).await.expect("unowned");
    assert!(world.records.issue(1).is_some(), "a source is released only once unowned for fifteen minutes");
    world.clock.advance(chrono::Duration::minutes(1));
    world.observer.release_unowned(&[]).await.expect("released");
    assert!(world.records.issue(1).is_none() && world.records.pull(5).is_none());
    assert!(world.states.sources().expect("sources").is_empty());
    assert!(world.records.whole.lock().expect("whole").is_empty());
}

#[tokio::test]
async fn file_states_survive_a_restart() {
    let directory = tempfile::tempdir().expect("directory");
    let state = SourceState {
        service: "github.com".into(),
        scope: "org/shared".into(),
        cursor: cursor(5),
        retry_issues: BTreeSet::from([3]),
        failing: true,
        gone_issues: BTreeSet::from([4]),
        inventoried_at: Some(at(5)),
        ..SourceState::default()
    };
    FileSourceStates::new(directory.path().into()).save(&state).expect("save");
    let reopened = FileSourceStates::new(directory.path().into());
    assert_eq!(reopened.load("github.com", "org/shared").expect("load"), Some(state));
    assert_eq!(reopened.sources().expect("sources"), [("github.com".to_string(), "org/shared".to_string())]);
    reopened.remove("github.com", "org/shared").expect("remove");
    assert_eq!(reopened.load("github.com", "org/shared").expect("load"), None);
}

#[tokio::test]
async fn an_undecodable_state_is_rebuilt_rather_than_fatal() {
    let directory = tempfile::tempdir().expect("directory");
    let states = FileSourceStates::new(directory.path().into());
    states.save(&SourceState { service: "github.com".into(), scope: "org/shared".into(), ..SourceState::default() }).expect("save");
    for entry in std::fs::read_dir(directory.path()).expect("list") {
        std::fs::write(entry.expect("entry").path(), b"not json").expect("corrupt");
    }
    assert_eq!(states.load("github.com", "org/shared").expect("load"), None, "the source is read again whole");
    assert!(states.sources().expect("sources").is_empty());
    for entry in std::fs::read_dir(directory.path()).expect("list") {
        std::fs::write(entry.expect("entry").path(), br#"{"service": "github.com", "scope": "org/shared", "cursor": 5}"#).expect("older");
    }
    assert_eq!(states.load("github.com", "org/shared").expect("load"), None);
    assert_eq!(states.sources().expect("sources"), [("github.com".into(), "org/shared".into())], "it is still released");
    let partial: SourceState = serde_json::from_str(r#"{"service": "github.com", "scope": "org/shared"}"#).expect("missing fields default");
    assert_eq!(partial.cursor, SourceCursor::default());
}

#[tokio::test]
async fn a_failed_local_read_writes_nothing_and_marks_nothing_failing() {
    let world = World::new().await;
    world.pass(Ok(feed(vec![issue(1, ObservedIssueState::Open)], vec![pull(5, ObservedChecks::Pass)]))).await.expect("pass");
    let (entries, saves) = (world.records.entries(), *world.states.saves.lock().expect("saves"));
    world.queries.refuse.store(true, std::sync::atomic::Ordering::SeqCst);
    let requests = world.feed.requests().len();
    assert!(matches!(world.pass_asking(Asks::default()).await, Err(PassError::Local(_))));
    assert_eq!(world.feed.requests().len(), requests, "the forge was not called");
    assert_eq!((world.records.entries(), *world.states.saves.lock().expect("saves")), (entries, saves));
    assert_eq!(world.records.issue(1).expect("issue").failing_since, None);
    assert!(!world.states.load("github.com", "org/shared").expect("load").expect("state").failing);
}

#[tokio::test]
async fn a_gone_item_is_removed_and_not_asked_for_again_unless_hinted() {
    let world = World::new().await;
    world.convoy_refers_to("worker", SubjectKind::Issue, 4).await;
    world.pass(Ok(feed(vec![issue(1, ObservedIssueState::Open), issue(4, ObservedIssueState::Open)], vec![]))).await.expect("pass");
    world.pass(Ok(FeedPass { gone: vec![(FeedItem::Issue, 4)], ..feed(vec![], vec![]) })).await.expect("gone");
    assert!(world.records.issue(4).is_none(), "a deleted issue is not held as open");
    world.pass(Ok(feed(vec![], vec![]))).await.expect("pass");
    assert!(world.feed.last_request().issues.is_empty(), "a gone item the convoy still names is not read again");
    world.pass_asking(Asks { issues: BTreeSet::from([4]), ..Asks::default() }).await.expect("hinted");
    assert_eq!(world.feed.last_request().issues, BTreeSet::from([4]), "a hint reads it again");
}

#[tokio::test]
async fn held_open_issues_are_checked_against_the_open_list_daily() {
    let world = World::new().await;
    world.pass(Ok(feed(vec![issue(1, ObservedIssueState::Open), issue(2, ObservedIssueState::Closed)], vec![]))).await.expect("pass");
    assert_eq!(world.feed.last_request().open_issues, Some(BTreeSet::new()), "the first pass checks the inventory");
    world.clock.advance(chrono::Duration::hours(23));
    world.pass(Ok(feed(vec![], vec![]))).await.expect("pass");
    assert_eq!(world.feed.last_request().open_issues, None);
    world.clock.advance(chrono::Duration::hours(1));
    world.pass(Ok(feed(vec![], vec![]))).await.expect("pass");
    assert_eq!(world.feed.last_request().open_issues, Some(BTreeSet::from([1])), "only open issues are checked");
}

#[tokio::test(start_paused = true)]
async fn the_scheduler_passes_each_source_on_its_cadence() {
    let world = World::running().await;
    world.feed.next(Ok(feed(vec![], vec![pull(5, ObservedChecks::Pending)])));
    let running = world.run();
    tokio::time::sleep(Duration::from_secs(40)).await;
    assert_eq!(world.feed.starts(), [0, 15, 30], "pending checks are read every fifteen seconds");
    assert!(world.feed.requests()[1..].iter().all(|request| request.pulls == BTreeSet::from([5])));
    world.feed.next(Ok(feed(vec![], vec![pull(5, ObservedChecks::Pass)])));
    tokio::time::sleep(Duration::from_secs(160)).await;
    assert_eq!(world.feed.starts(), [0, 15, 30, 45, 105, 165], "then every minute");
    running.abort();
}

#[tokio::test(start_paused = true)]
async fn a_hint_passes_its_source_early_and_reads_the_hinted_item() {
    let world = World::running().await;
    let running = world.run();
    tokio::time::sleep(Duration::from_secs(10)).await;
    world.observer.hint(&world.address(Kind::ChangeRequest, 8).await);
    world.observer.hint(&world.address(Kind::Issue, 9).await);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(world.feed.starts(), [0, 10]);
    let request = world.feed.last_request();
    assert_eq!((request.issues, request.pulls), (BTreeSet::from([9]), BTreeSet::from([8])));
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(world.feed.starts(), [0, 10, 70], "the cadence resumes from the hinted pass");
    assert!(world.feed.last_request().pulls.is_empty(), "a hinted item read once is not asked for again");
    running.abort();
}

#[tokio::test(start_paused = true)]
async fn a_healthy_relay_slows_only_pending_check_reads_and_a_resync_passes_every_source() {
    let world = World::running().await;
    world.observer.set_relay_healthy(true);
    world.feed.next(Ok(feed(vec![issue(1, ObservedIssueState::Open)], vec![pull(5, ObservedChecks::Pending)])));
    let running = world.run();
    tokio::time::sleep(Duration::from_secs(130)).await;
    assert_eq!(world.feed.starts(), [0, 60, 120], "issue edits keep the minute cadence");
    assert!(world.feed.requests()[1..].iter().all(|request| request.pulls.is_empty()), "the relay hints check changes");

    world.observer.wake_all();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(world.feed.starts(), [0, 60, 120, 130], "a resync passes the source now");
    assert_eq!(world.feed.last_request().pulls, BTreeSet::from([5]), "and reads pending checks again");

    tokio::time::sleep(Duration::from_secs(900)).await;
    let requests = world.feed.requests();
    let pending = requests.iter().filter(|request| request.pulls.contains(&5)).map(|request| (request.started - at(0)).num_seconds());
    assert_eq!(pending.collect::<Vec<_>>(), [130, 1030], "pending checks are read again every fifteen minutes");
    running.abort();
}

#[tokio::test]
async fn declarations_briefly_missing_release_nothing() {
    let world = World::new().await;
    world.pass(Ok(feed(vec![issue(1, ObservedIssueState::Open)], vec![]))).await.expect("pass");
    let project = world.backend.using::<Project>(NS).get("shared").await.expect("project");
    world.backend.using::<Project>(NS).delete("shared").await.expect("declarations not yet replicated");
    world.observer.observe_owned().await.expect("nothing owned");
    world.clock.advance(chrono::Duration::minutes(10));
    world.observer.observe_owned().await.expect("nothing owned");
    assert!(world.records.issue(1).is_some(), "nothing is deleted while the declarations catch up");
    world.backend.using::<Project>(NS).create(&InputMeta::builder().name("shared".into()).build(), &project.spec).await.expect("back");
    world.observer.observe_owned().await.expect("owned again");
    world.backend.using::<Project>(NS).delete("shared").await.expect("gone again");
    world.clock.advance(chrono::Duration::minutes(10));
    world.observer.observe_owned().await.expect("nothing owned");
    assert!(world.records.issue(1).is_some(), "the grace restarts once the source is owned again");
    assert!(world.records.whole.lock().expect("whole").contains(&("github.com".into(), "org/shared".into())));
}

#[tokio::test]
async fn a_source_whose_state_cannot_be_saved_writes_nothing() {
    let world = World::new().await;
    world.states.refuse.store(true, std::sync::atomic::Ordering::SeqCst);
    world.feed.next(Ok(feed(vec![issue(1, ObservedIssueState::Open)], vec![])));
    assert!(matches!(world.pass_asking(Asks::default()).await, Err(PassError::Local(_))));
    assert_eq!(world.records.entries(), 0, "a host writes records only for a source its release can find");
    assert!(world.feed.requests().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failed_pass_waits_for_its_backoff_whatever_wakes_the_loop() {
    let world = World::running().await;
    let running = world.run();
    tokio::time::sleep(Duration::from_secs(10)).await;
    world.feed.next(Err("forge down".into()));
    world.observer.hint(&world.address(Kind::ChangeRequest, 8).await);
    tokio::time::sleep(Duration::from_secs(10)).await;
    let elsewhere = flotilla_resources::address::stored_repository_address("github.com", "org/other", &[]).expect("repository");
    world.observer.hint(&elsewhere.child(Kind::ChangeRequest, "3").expect("address"));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(world.feed.starts(), [0, 10], "a hint for another source does not retry this one early");
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(world.feed.starts(), [0, 10, 70]);
    assert_eq!(world.feed.last_request().pulls, BTreeSet::from([8]), "the failed pass's hinted item is asked for again");
    running.abort();
}
