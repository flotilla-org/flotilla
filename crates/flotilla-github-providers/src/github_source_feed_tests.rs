use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_forge_github::github::GhApiClient;
use flotilla_issue_tracker::source_feed::{Fact, FeedItem, FeedRequest, SourceFeed};
use flotilla_protocol::{IssueRef, IssueSource};
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};
use flotilla_resources::{ObservedChangeRequestState, ObservedChecks, ObservedIssueState};
use serde_json::{json, Value};

use crate::issue_tracker::GitHubIssueProvider;

enum Expect {
    Rest(Vec<String>),
    Graphql { issues: Vec<u64>, pulls: Vec<u64> },
}

/// The gh boundary: each request must be the next one expected, in its documented shape; a
/// GraphQL batch must name exactly the expected items.
struct Gh {
    script: Mutex<VecDeque<(Expect, String)>>,
}

impl Gh {
    fn new() -> Arc<Self> {
        Arc::new(Self { script: Mutex::new(VecDeque::new()) })
    }

    fn rest(&self, args: &[&str], response: String) {
        let args = std::iter::once("api").chain(std::iter::once("--include")).chain(args.iter().copied()).map(str::to_string).collect();
        self.script.lock().expect("script").push_back((Expect::Rest(args), response));
    }

    fn graphql(&self, issues: &[u64], pulls: &[u64], repository: Value) {
        let response = ok(
            None,
            &json!({"data": {"rateLimit": {"cost": 1, "remaining": 4999, "resetAt": "2099-01-01T00:00:00Z"}, "repository": repository}}),
        );
        self.script.lock().expect("script").push_back((Expect::Graphql { issues: issues.to_vec(), pulls: pulls.to_vec() }, response));
    }

    fn finished(&self) {
        assert!(self.script.lock().expect("script").is_empty(), "expected requests were not made");
    }
}

fn aliases(query: &str, prefix: &str) -> Vec<u64> {
    query.split(' ').filter_map(|word| word.strip_prefix(prefix)?.strip_suffix(':')?.parse().ok()).collect()
}

#[async_trait]
impl CommandRunner for Gh {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        self.run_output(cmd, args, cwd, label).await.map(|output| output.stdout)
    }

    async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        assert_eq!(cmd, "gh");
        let (expected, response) = self.script.lock().expect("script").pop_front().unwrap_or_else(|| panic!("unexpected request {args:?}"));
        match expected {
            Expect::Rest(expected) => assert_eq!(args, expected),
            Expect::Graphql { issues, pulls } => {
                assert_eq!(&args[..4], &["api", "graphql", "--include", "-f"]);
                let query = args[4].strip_prefix("query=").expect("query argument");
                assert!(query.contains("repository(owner:\"team\",name:\"repo\")"));
                assert_eq!((aliases(query, "issue"), aliases(query, "pr")), (issues, pulls));
            }
        }
        let succeeded = response.starts_with("HTTP/2 2") || response.starts_with("HTTP/2 304");
        Ok(CommandOutput { stdout: response, stderr: String::new(), exit_code: Some(if succeeded { 0 } else { 1 }) })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}

fn ok(etag: Option<&str>, body: &Value) -> String {
    let etag = etag.map(|etag| format!("ETag: {etag}\r\n")).unwrap_or_default();
    format!("HTTP/2 200 OK\r\n{etag}\r\n{body}")
}

fn status(line: &str) -> String {
    format!("HTTP/2 {line}\r\n\r\n{{\"message\": \"{line}\"}}")
}

fn not_modified() -> String {
    "HTTP/2 304 Not Modified\r\n\r\n".into()
}

const UPDATED: &str = "2026-10-07T12:00:00Z";

fn list(state: &str, kind: &str) -> String {
    format!("repos/team/repo/{kind}?state={state}&sort=updated&direction=desc&per_page=100&page=1")
}

fn issue(number: u64, state: &str) -> Value {
    json!({
        "number": number, "title": format!("Issue {number}"), "state": state, "body": "The contract.",
        "updated_at": UPDATED, "labels": [{"name": "ready"}], "assignees": [{"login": "coder"}],
        "issue_field_values": [
            {"issue_field_name": "Priority", "value": 1, "single_select_option": {"name": "High"}},
            {"issue_field_name": "Weight", "value": 2.5},
            {"issue_field_name": "Due", "value": null}
        ]
    })
}

fn pull(number: u64) -> Value {
    json!({"number": number, "updated_at": UPDATED, "state": "open"})
}

fn issue_detail() -> Value {
    json!({
        "blockedBy": {"totalCount": 1, "nodes": [{"url": "https://github.com/team/repo/issues/2"}]},
        "parent": {"url": "https://github.com/team/epics/issues/3"},
        "issueType": {"name": "Task"}
    })
}

fn pull_detail(check: &str) -> Value {
    json!({
        "title": "Observer", "state": "OPEN", "isDraft": false, "headRefOid": "abc", "headRefName": "feature",
        "baseRefName": "main", "reviewDecision": null, "mergeable": "MERGEABLE", "author": {"login": "coder"},
        "closingIssuesReferences": {"nodes": [{"url": "https://github.com/team/repo/issues/1"}], "pageInfo": {"hasNextPage": false}},
        "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [{"status": check, "conclusion": null}]}}}}]}
    })
}

fn source() -> IssueSource {
    IssueSource { service: "https://github.com".into(), scope: "team/repo".into() }
}

fn provider(gh: &Arc<Gh>) -> GitHubIssueProvider {
    let runner: Arc<dyn CommandRunner> = gh.clone();
    GitHubIssueProvider::new(Arc::new(GhApiClient::new(Arc::clone(&runner))), runner, Path::new("/"))
}

fn reference(scope: &str, id: &str) -> IssueRef {
    IssueRef { source: IssueSource { service: "https://github.com".into(), scope: scope.into() }, id: id.into() }
}

#[tokio::test]
async fn passes_read_the_lists_from_the_cursor_and_detail_only_changed_or_pending_items() {
    let gh = Gh::new();
    let feed = provider(&gh);

    gh.rest(&[&list("open", "issues")], ok(Some("i1"), &json!([issue(1, "open")])));
    gh.rest(&[&list("open", "pulls")], ok(Some("p1"), &json!([pull(5)])));
    gh.graphql(&[1], &[5], json!({"issue1": issue_detail(), "pr5": pull_detail("IN_PROGRESS")}));
    let first = feed.pass(&source(), &FeedRequest::default()).await.expect("first pass");
    let [read] = first.issues.as_slice() else { panic!("one issue: {first:?}") };
    assert_eq!((read.number, read.state, read.title.as_str()), (1, ObservedIssueState::Open, "Issue 1"));
    assert_eq!(
        (read.labels.as_slice(), read.assignees.as_slice(), read.body.as_deref()),
        (&["ready".to_string()][..], &["coder".to_string()][..], Some("The contract."))
    );
    assert_eq!(read.blocked_by, Fact::Read(vec![reference("team/repo", "2")]));
    assert_eq!(read.parent, Fact::Read(Some(reference("team/epics", "3"))));
    assert_eq!(read.issue_type, Fact::Read(Some("Task".into())));
    assert_eq!(read.fields, Fact::Read(BTreeMap::from([("Priority".into(), "High".into()), ("Weight".into(), "2.5".into())])));
    let [pr] = first.pulls.as_slice() else { panic!("one pull request: {first:?}") };
    assert_eq!((pr.number, pr.state, pr.checks.clone()), (5, ObservedChangeRequestState::Open, Fact::Read(Some(ObservedChecks::Pending))));
    assert_eq!(
        (pr.head_branch.as_deref(), pr.base_branch.as_deref(), pr.head_sha.as_deref()),
        (Some("feature"), Some("main"), Some("abc"))
    );
    assert_eq!(pr.closes, Fact::Read(vec![reference("team/repo", "1")]));
    assert!(first.unread.is_empty());

    let since = format!("{}&since=2026-10-07T11%3A59%3A59Z", list("all", "issues"));
    gh.rest(&[&list("all", "issues")], ok(Some("i2"), &json!([issue(1, "open")])));
    gh.rest(&[&since], ok(None, &json!([issue(1, "open")])));
    gh.rest(&[&list("all", "pulls")], ok(Some("p2"), &json!([pull(5)])));
    gh.graphql(&[], &[5], json!({"pr5": pull_detail("COMPLETED")}));
    let pending = FeedRequest { cursor: first.cursor.clone(), pulls: BTreeSet::from([5]), ..Default::default() };
    let second = feed.pass(&source(), &pending).await.expect("second pass");
    assert!(second.issues.is_empty(), "an item already reported is not read again");
    assert_eq!(second.pulls.iter().map(|pr| pr.number).collect::<Vec<_>>(), [5], "a pending pull request is read again");
    assert_eq!(second.cursor, first.cursor);

    gh.rest(&[&list("all", "issues"), "-H", "If-None-Match: i2"], not_modified());
    gh.rest(&[&list("all", "pulls"), "-H", "If-None-Match: p2"], not_modified());
    let quiet = feed.pass(&source(), &FeedRequest { cursor: second.cursor.clone(), ..Default::default() }).await.expect("quiet pass");
    assert!(quiet.issues.is_empty() && quiet.pulls.is_empty() && quiet.unread.is_empty());
    assert_eq!(quiet.cursor, second.cursor);
    gh.finished();
}

#[tokio::test]
async fn a_requested_item_the_lists_omit_is_read_whole_and_a_missing_one_is_unread() {
    let gh = Gh::new();
    let feed = provider(&gh);
    gh.rest(&[&list("open", "issues")], ok(None, &json!([])));
    gh.rest(&[&list("open", "pulls")], ok(None, &json!([])));
    gh.rest(&["repos/team/repo/issues/9"], ok(None, &issue(9, "closed")));
    gh.rest(&["repos/team/repo/issues/12"], ok(None, &json!({"number": 12, "updated_at": UPDATED, "pull_request": {}})));
    gh.graphql(&[9], &[7], json!({"issue9": issue_detail(), "pr7": null}));
    let request = FeedRequest { issues: BTreeSet::from([9, 12]), pulls: BTreeSet::from([7]), ..Default::default() };
    let pass = feed.pass(&source(), &request).await.expect("pass");
    assert_eq!(pass.issues.iter().map(|issue| (issue.number, issue.state)).collect::<Vec<_>>(), [(9, ObservedIssueState::Closed)]);
    let unread = pass.unread.iter().map(|(item, number, _)| (*item, *number)).collect::<Vec<_>>();
    assert_eq!(unread, [(FeedItem::Issue, 12), (FeedItem::ChangeRequest, 7)]);
    assert!(pass.cursor.issues.is_some_and(|cursor| cursor.seen.is_empty()), "an empty first list starts from the pass");
    gh.finished();
}

#[tokio::test]
async fn an_issue_github_lacks_deleted_or_moved_is_gone() {
    let gh = Gh::new();
    let feed = provider(&gh);
    gh.rest(&[&list("open", "issues")], ok(Some("i1"), &json!([issue(1, "open")])));
    gh.rest(&[&list("open", "pulls")], ok(None, &json!([])));
    gh.rest(&[&list("open", "issues"), "-H", "If-None-Match: i1"], not_modified());
    gh.rest(&["repos/team/repo/issues/3"], status("404 Not Found"));
    gh.rest(&["repos/team/repo/issues/4"], status("410 Gone"));
    let mut moved = issue(6, "open");
    moved["repository_url"] = json!("https://api.github.com/repos/team/elsewhere");
    gh.rest(&["repos/team/repo/issues/6"], ok(None, &moved));
    gh.rest(&["repos/team/repo/issues/8"], status("502 Bad Gateway"));
    gh.graphql(&[1], &[], json!({"issue1": issue_detail()}));
    let request = FeedRequest { issues: BTreeSet::from([3, 8]), open_issues: Some(BTreeSet::from([1, 4, 6])), ..Default::default() };
    let pass = feed.pass(&source(), &request).await.expect("pass");
    assert_eq!(pass.gone, [(FeedItem::Issue, 3), (FeedItem::Issue, 4), (FeedItem::Issue, 6)]);
    assert_eq!(pass.unread.iter().map(|(item, number, _)| (*item, *number)).collect::<Vec<_>>(), [(FeedItem::Issue, 8)]);
    assert_eq!(pass.issues.iter().map(|issue| issue.number).collect::<Vec<_>>(), [1], "a held issue still open is not read directly");
    gh.finished();
}

#[tokio::test]
async fn batches_hold_at_most_twenty_items() {
    let gh = Gh::new();
    let feed = provider(&gh);
    let issues = (1..=25).map(|number| issue(number, "open")).collect::<Vec<_>>();
    gh.rest(&[&list("open", "issues")], ok(None, &json!(issues)));
    gh.rest(&[&list("open", "pulls")], ok(None, &json!([pull(30)])));
    let first = (1..=20).collect::<Vec<_>>();
    gh.graphql(&first, &[], Value::Object(first.iter().map(|number| (format!("issue{number}"), issue_detail())).collect()));
    let rest = (21..=25).collect::<Vec<_>>();
    let mut repository = rest.iter().map(|number| (format!("issue{number}"), issue_detail())).collect::<serde_json::Map<_, _>>();
    repository.insert("pr30".into(), pull_detail("COMPLETED"));
    gh.graphql(&rest, &[30], Value::Object(repository));
    let pass = feed.pass(&source(), &FeedRequest::default()).await.expect("pass");
    assert_eq!((pass.issues.len(), pass.pulls.len()), (25, 1));
    gh.finished();
}

#[tokio::test]
async fn a_truncated_link_window_is_unread_rather_than_short() {
    let gh = Gh::new();
    let feed = provider(&gh);
    gh.rest(&[&list("open", "issues")], ok(None, &json!([issue(1, "open")])));
    gh.rest(&[&list("open", "pulls")], ok(None, &json!([])));
    let mut detail = issue_detail();
    detail["blockedBy"]["totalCount"] = json!(101);
    gh.graphql(&[1], &[], json!({"issue1": detail}));
    let pass = feed.pass(&source(), &FeedRequest::default()).await.expect("pass");
    assert_eq!(pass.issues[0].blocked_by, Fact::Unread);
    gh.finished();
}
