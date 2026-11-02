//! A source observed whole through the engine: the preferred observer's passes over a Forgejo
//! stand-in write one record per issue and per change request, and a pass that sees nothing new
//! writes no log entry.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use flotilla_fake_forge::{
    recorded::ManualClock, Clock as _, FakeForge, ForgeSettings, InProcessClient, ItemEdit, NewIssue, NewPull, NewReview, RepoRef,
    ReviewState,
};
use flotilla_forge_forgejo::{ForgejoAuth, ForgejoIssueProviderConfig};
use flotilla_forgejo_providers::issue_tracker::ForgejoIssueProvider;
use flotilla_issue_tracker::IssueProvider;
use flotilla_observation::{
    source_observer::{FileSourceStates, SourceObserver, SourceObserverParts},
    sources::IssueQueryPort,
};
use flotilla_protocol::{IssueSource, NodeId};
use flotilla_provisioning::engine::observations::EngineObservations;
use flotilla_replay_testkit::testing::MockRunner;
use flotilla_resources::{
    forge_observation::{ChangeRequestState, ReviewDecision, SourceRecords},
    Clock, Forge, ForgeKind, ForgeSpec, InputMeta, IssueSourceBindingSpec, Project, ProjectSpec, SystemClock,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NS: &str = "flotilla";
const TOKEN: &str = "fake-forge-token";

struct Queries(Arc<ForgejoIssueProvider>);

#[async_trait]
impl IssueQueryPort for Queries {
    async fn provider_for_source(&self, source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
        assert!(self.0.supports(source), "the observer asks for its own source");
        Ok(self.0.clone())
    }
}

struct World {
    _directory: tempfile::TempDir,
    clock: Arc<ManualClock>,
    fake: Arc<FakeForge>,
    repo: RepoRef,
    service: Arc<Service>,
    records: Arc<EngineObservations>,
    observer: SourceObserver,
}

impl World {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("directory");
        let clock = Arc::new(ManualClock::new("2026-11-02T00:00:00Z".parse().expect("time")));
        let fake = Arc::new(FakeForge::new("https://fake-forge.test:3443", clock.clone()));
        fake.add_forge("lab", ForgeSettings { token: TOKEN.into(), login: "flotilla-bot".into() }).expect("forge");
        let service_url = fake.service_url("lab");

        let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("owner"));
        let host = service_url.trim_start_matches("https://").split(['/', ':']).next().expect("host").to_string();
        let forge = ForgeSpec {
            forge_id: "lab".into(),
            kind: ForgeKind::Forgejo,
            hosts: [host.clone()].into(),
            https_url: service_url.clone(),
            git_ssh_host: host,
        };
        backend.using::<Forge>(NS).create(&InputMeta::builder().name("lab".into()).build(), &forge).await.expect("forge record");
        let source = IssueSource { service: service_url.clone(), scope: "team/repo".into() };
        let project = ProjectSpec::builder()
            .display_name("Team".into())
            .issue_source_bindings(vec![IssueSourceBindingSpec::builder().source(source).alias("team".into()).build()])
            .build();
        backend.using::<Project>(NS).create(&InputMeta::builder().name("team".into()).build(), &project).await.expect("project");

        let mut kinds = flotilla_schema::KindRegistry::new();
        flotilla_provisioning::stored::forge_observation::register(&mut kinds).expect("register");
        let service = Arc::new(Service::open(&directory.path().join("owner.sqlite"), "owner", kinds).expect("store"));
        let records = Arc::new(EngineObservations::open(Arc::clone(&service), Arc::new(SystemClock) as Arc<dyn Clock>).expect("records"));

        let feed = ForgejoIssueProvider::new(
            Arc::new(InProcessClient::new(Arc::clone(&fake))),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(service_url, None, ForgejoAuth { token: TOKEN.into(), token_path: PathBuf::from("token") }),
        );
        let observer = SourceObserver::new(
            SourceObserverParts::builder()
                .backend(backend)
                .namespace(NS.into())
                .queries(Arc::new(Queries(Arc::new(feed))))
                .records(records.clone() as Arc<dyn SourceRecords>)
                .states(Arc::new(FileSourceStates::new(directory.path().join("forge-sources"))))
                .clock(Arc::new(SystemClock))
                .build(),
        );
        Self { _directory: directory, clock, fake, repo: RepoRef::new("lab", "team", "repo"), service, records, observer }
    }

    fn tick(&self) {
        self.clock.set(self.clock.now() + chrono::Duration::minutes(1));
    }

    fn last_sequence(&self) -> u64 {
        self.service.with_store(|store| store.read()?.last_sequence("owner")).expect("sequence")
    }

    fn entries_since(&self, after: u64) -> Vec<String> {
        let events = self.service.with_store(|store| store.read()?.changes_since("owner", after, 100)).expect("entries");
        events.into_iter().map(|event| event.name).collect()
    }
}

#[tokio::test]
async fn a_repeated_unchanged_pass_writes_no_log_entry() {
    let world = World::new().await;
    world.tick();
    let issue = NewIssue { title: "Observer".into(), labels: vec!["ready".into()], ..NewIssue::default() };
    let issue = world.fake.create_issue(&world.repo, issue).expect("issue").number;
    world.tick();
    let pull =
        NewPull { head: "feature".into(), base: "main".into(), title: "Passes".into(), body: String::new(), user: None, draft: false };
    let pull = world.fake.create_pull(&world.repo, pull).expect("pull").number;
    let review = NewReview { user: "reviewer".into(), state: ReviewState::Approved, body: String::new() };
    world.fake.add_review(&world.repo, pull, review).expect("review");

    world.observer.observe_owned().await.expect("first pass");
    let issues = world.records.issues("lab", "team/repo").await.expect("issues");
    let pulls = world.records.change_requests("lab", "team/repo").await.expect("pulls");
    assert_eq!(
        issues.iter().map(|held| (held.record.number, held.record.title.as_deref())).collect::<Vec<_>>(),
        [(issue, Some("Observer"))]
    );
    let [held] = pulls.as_slice() else { panic!("one change request: {pulls:?}") };
    assert_eq!(
        (held.record.number, held.record.state, held.record.review_decision),
        (pull, Some(ChangeRequestState::Open), Some(ReviewDecision::Approved))
    );
    assert_eq!((held.record.head_branch.as_deref(), held.record.base_branch.as_deref()), (Some("feature"), Some("main")));

    let written = world.last_sequence();
    for _ in 0..3 {
        world.observer.observe_owned().await.expect("unchanged pass");
    }
    assert_eq!(world.last_sequence(), written, "a pass that sees nothing new writes no log entry");

    world.tick();
    world.fake.edit(&world.repo, issue, ItemEdit { title: Some("Observer, retitled".into()), ..ItemEdit::default() }).expect("edit");
    world.observer.observe_owned().await.expect("changed pass");
    assert_eq!(world.entries_since(written), [issues[0].name.clone()], "one entry, for the record whose value changed");
    let (refusals, count) = world.fake.refusals();
    assert_eq!(count, 0, "the fake refused observer requests: {refusals:?}");
}
