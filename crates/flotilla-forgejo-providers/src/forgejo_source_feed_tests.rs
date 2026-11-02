use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use flotilla_fake_forge::{
    recorded::ManualClock, Clock, FakeForge, ForgeSettings, InProcessClient, ItemEdit, ItemState, NewIssue, NewPull, NewReview, RepoRef,
    ReviewState,
};
use flotilla_forge_forgejo::{ForgejoAuth, ForgejoIssueProviderConfig};
use flotilla_issue_tracker::source_feed::{Fact, FeedItem, FeedRequest, SourceFeed};
use flotilla_protocol::IssueSource;
use flotilla_replay_testkit::testing::MockRunner;
use flotilla_resources::{ObservedChangeRequestState, ObservedIssueState, ObservedReviewDecision};

use crate::issue_tracker::ForgejoIssueProvider;

const URL: &str = "https://fake-forge.test:3443";
const TOKEN: &str = "fake-forge-token";

struct World {
    clock: Arc<ManualClock>,
    fake: Arc<FakeForge>,
    repo: RepoRef,
    feed: ForgejoIssueProvider,
    source: IssueSource,
}

impl World {
    fn new() -> Self {
        let clock = Arc::new(ManualClock::new("2026-11-02T00:00:00Z".parse().expect("time")));
        let fake = Arc::new(FakeForge::new(URL, clock.clone()));
        fake.add_forge("lab", ForgeSettings { token: TOKEN.into(), login: "flotilla-bot".into() }).expect("forge");
        let repo = RepoRef::new("lab", "team", "repo");
        let feed = ForgejoIssueProvider::new(
            Arc::new(InProcessClient::new(Arc::clone(&fake))),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(
                fake.service_url("lab"),
                None,
                ForgejoAuth { token: TOKEN.into(), token_path: PathBuf::from("fake-forge-token") },
            ),
        );
        let source = IssueSource { service: fake.service_url("lab"), scope: "team/repo".into() };
        Self { clock, fake, repo, feed, source }
    }

    fn tick(&self) {
        self.clock.set(self.clock.now() + chrono::Duration::minutes(1));
    }

    fn issue(&self, title: &str) -> u64 {
        self.tick();
        let new = NewIssue { title: title.into(), body: "The contract.".into(), labels: vec!["ready".into()], ..NewIssue::default() };
        self.fake.create_issue(&self.repo, new).expect("issue").number
    }

    fn assert_no_refusals(&self) {
        let (refusals, count) = self.fake.refusals();
        assert_eq!(count, 0, "the fake refused feed requests: {refusals:?}");
    }
}

#[tokio::test]
async fn forgejo_passes_list_from_the_cursor_and_read_only_what_changed() {
    let world = World::new();
    let closed = world.issue("Finished work");
    world.tick();
    world.fake.edit(&world.repo, closed, ItemEdit { state: Some(ItemState::Closed), ..ItemEdit::default() }).expect("close");
    let open = world.issue("Open work");
    world.tick();
    let new_pull =
        NewPull { head: "feature".into(), base: "main".into(), title: "Observer".into(), body: String::new(), user: None, draft: false };
    let pull = world.fake.create_pull(&world.repo, new_pull).expect("pull").number;
    let review = NewReview { user: "reviewer".into(), state: ReviewState::Approved, body: String::new() };
    world.fake.add_review(&world.repo, pull, review).expect("review");

    let request = FeedRequest { issues: BTreeSet::from([closed]), ..Default::default() };
    let first = world.feed.pass(&world.source, &request).await.expect("first pass");
    let issues = first.issues.iter().map(|issue| (issue.number, issue.state)).collect::<Vec<_>>();
    assert_eq!(issues, [(open, ObservedIssueState::Open), (closed, ObservedIssueState::Closed)]);
    let read = &first.issues[0];
    assert_eq!((read.labels.as_slice(), read.body.as_deref()), (&["ready".to_string()][..], Some("The contract.")));
    assert_eq!((&read.blocked_by, &read.parent, &read.fields), (&Fact::Unread, &Fact::Unread, &Fact::Unread));
    let [pr] = first.pulls.as_slice() else { panic!("one pull request: {first:?}") };
    assert_eq!((pr.number, pr.state), (pull, ObservedChangeRequestState::Open));
    assert_eq!((pr.head_branch.as_deref(), pr.base_branch.as_deref()), (Some("feature"), Some("main")));
    assert_eq!(pr.review_decision, Fact::Read(Some(ObservedReviewDecision::Approved)));
    assert_eq!((&pr.checks, &pr.closes), (&Fact::Unread, &Fact::Unread));

    let quiet = world.feed.pass(&world.source, &FeedRequest { cursor: first.cursor.clone(), ..Default::default() }).await.expect("quiet");
    assert!(quiet.issues.is_empty() && quiet.pulls.is_empty() && quiet.unread.is_empty(), "{quiet:?}");
    assert_eq!(quiet.cursor, first.cursor);

    world.tick();
    world.fake.edit(&world.repo, open, ItemEdit { title: Some("Retitled".into()), ..ItemEdit::default() }).expect("edit");
    let changed = world.feed.pass(&world.source, &FeedRequest { cursor: quiet.cursor.clone(), ..Default::default() }).await.expect("pass");
    assert_eq!(changed.issues.iter().map(|issue| (issue.number, issue.title.as_str())).collect::<Vec<_>>(), [(open, "Retitled")]);
    assert!(changed.pulls.is_empty(), "an unchanged pull request is not read again");
    world.assert_no_refusals();
}

#[tokio::test]
async fn a_requested_item_that_is_missing_or_a_pull_is_unread() {
    let world = World::new();
    let new_pull =
        NewPull { head: "feature".into(), base: "main".into(), title: "Observer".into(), body: String::new(), user: None, draft: false };
    let pull = world.fake.create_pull(&world.repo, new_pull).expect("pull").number;
    let request = FeedRequest { issues: BTreeSet::from([pull, 40]), pulls: BTreeSet::from([41]), ..Default::default() };
    let pass = world.feed.pass(&world.source, &request).await.expect("pass");
    let unread = pass.unread.iter().map(|(item, number, _)| (*item, *number)).collect::<Vec<_>>();
    assert_eq!(unread, [(FeedItem::Issue, pull), (FeedItem::Issue, 40), (FeedItem::ChangeRequest, 41)]);
    assert_eq!(pass.pulls.iter().map(|pr| pr.number).collect::<Vec<_>>(), [pull]);
}
