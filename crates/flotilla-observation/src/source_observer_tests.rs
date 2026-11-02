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
    FeedPass { cursor: cursor(0), issues, pulls, unread: vec![] }
}

/// This home's records: written only when a value changes, each write counted as one log entry.
struct Records {
    clock: Arc<VirtualClock>,
    issues: Mutex<BTreeMap<String, (IssueRecord, Timestamp)>>,
    pulls: Mutex<BTreeMap<String, (ChangeRequestRecord, Timestamp)>>,
    entries: Mutex<usize>,
    whole: Mutex<BTreeSet<(String, String)>>,
}

impl Records {
    fn new(clock: Arc<VirtualClock>) -> Arc<Self> {
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
}

impl SourceStates for States {
    fn load(&self, service: &str, scope: &str) -> Result<Option<SourceState>, String> {
        Ok(self.saved.lock().expect("states").get(&(service.into(), scope.into())).cloned())
    }

    fn save(&self, state: &SourceState) -> Result<(), String> {
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

/// The forge's feed for the test source: each pass answers with the next scripted result and
/// records what it was asked for.
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
}

#[async_trait]
impl SourceFeed for Feed {
    async fn pass(&self, source: &IssueSource, request: &FeedRequest) -> Result<FeedPass, String> {
        assert_eq!(source, &feed_source());
        self.requests.lock().expect("requests").push(request.clone());
        self.script.lock().expect("script").pop_front().expect("a scripted pass")
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

struct Queries(Arc<Feed>);

#[async_trait]
impl IssueQueryPort for Queries {
    async fn provider_for_source(&self, source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
        assert_eq!(source, &feed_source());
        Ok(Arc::new(Provider(Arc::clone(&self.0))))
    }
}

struct World {
    backend: ResourceBackend,
    clock: Arc<VirtualClock>,
    records: Arc<Records>,
    states: Arc<States>,
    feed: Arc<Feed>,
    observer: SourceObserver,
}

impl World {
    async fn new() -> Self {
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("owner"));
        let spec = ProjectSpec::builder()
            .display_name("Shared".into())
            .issue_source_bindings(vec![IssueSourceBindingSpec::builder().source(feed_source()).alias("shared".into()).build()])
            .build();
        backend.using::<Project>(NS).create(&InputMeta::builder().name("shared".into()).build(), &spec).await.expect("project");
        let clock = Arc::new(VirtualClock::new(at(0)));
        let records = Records::new(Arc::clone(&clock));
        let states = Arc::new(States::default());
        let feed = Arc::new(Feed::default());
        let observer = SourceObserver::new(
            SourceObserverParts::builder()
                .backend(backend.clone())
                .namespace(NS.into())
                .queries(Arc::new(Queries(Arc::clone(&feed))))
                .records(records.clone())
                .states(states.clone())
                .clock(clock.clone())
                .build(),
        );
        Self { backend, clock, records, states, feed, observer }
    }

    async fn source(&self) -> Observed {
        let owned = self.observer.owned().await.expect("owned");
        let [source] = owned.as_slice() else { panic!("one owned source: {owned:?}") };
        source.clone()
    }

    async fn pass(&self, pass: Result<FeedPass, String>) -> Result<(), String> {
        self.feed.next(pass);
        let source = self.source().await;
        self.observer.pass(&source).await.map(|_| ())
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
    let elsewhere = SourceObserver::new(
        SourceObserverParts::builder()
            .backend(ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("other")))
            .namespace(NS.into())
            .queries(Arc::new(Queries(Arc::clone(&world.feed))))
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
        retry_pulls: BTreeSet::new(),
        failing: true,
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
    let partial: SourceState = serde_json::from_str(r#"{"service": "github.com", "scope": "org/shared"}"#).expect("missing fields default");
    assert_eq!(partial.cursor, SourceCursor::default());
}
