use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{GitHubIssueProvider, BOARD_BATCH_SIZE};

use crate::providers::{
    github_api::{rate_limit_reset, GhApiClient},
    github_poll::BoardState,
    issue_tracker::IssueProvider,
    ChannelLabel, CommandOutput, CommandRunner,
};
use flotilla_protocol::IssueSource;

// Stand-in for the gh/HTTP boundary. Enforce the actual REST routes and
// bounded GraphQL selection, and return GitHub's connection-shaped responses.
#[derive(bon::Builder)]
struct BoardForge {
    count: usize,
    #[builder(default)]
    calls: AtomicUsize,
    #[builder(default)]
    completed: AtomicUsize,
    #[builder(default = 5000)]
    remaining: u64,
    #[builder(default)]
    invalid: Mutex<Option<&'static str>>,
}
impl BoardForge {
    fn new(count: usize) -> Self {
        Self::builder().count(count).build()
    }
}
#[async_trait]
impl CommandRunner for BoardForge {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        self.run_output(cmd, args, cwd, label).await.map(|output| output.stdout)
    }
    async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        assert_eq!(cmd, "gh");
        assert_eq!(args[0], "api");
        let body = if args[1] == "--include" {
            let endpoint = args[2];
            assert!(endpoint.starts_with("repos/team/repo/"));
            assert!(endpoint.contains("state=open&sort=updated&direction=desc&per_page=100&page="));
            if endpoint.contains("/pulls?") {
                json!([])
            } else {
                let page = endpoint.rsplit("page=").next().expect("page").parse::<usize>().expect("page number");
                let start = (page - 1) * 100;
                let items: Vec<_> = (start..(start + 100).min(self.count))
                    .map(|i| json!({"number":i+1,"updated_at":"2026-10-08T00:00:00Z","state":"open"}))
                    .collect();
                let link = if start + 100 < self.count { "Link: <next>; rel=\"next\"\r\n" } else { "" };
                return Ok(CommandOutput {
                    stdout: format!("HTTP/2 200 OK\r\n{link}\r\n{}", json!(items)),
                    stderr: String::new(),
                    exit_code: Some(0),
                });
            }
        } else {
            assert_eq!(&args[..4], &["api", "graphql", "--include", "-f"]);
            let query = args[4].strip_prefix("query=").expect("query argument");
            assert!(query.contains("repository(owner:\"team\",name:\"repo\")"));
            for field in [
                "rateLimit { cost remaining resetAt }",
                "labels(first:100)",
                "blockedBy(first:100)",
                "closedByPullRequestsReferences(first:100,includeClosedPrs:true)",
                "parent { url }",
                "issueType { name }",
            ] {
                assert!(query.contains(field), "missing native selection: {field}");
            }
            let ids: Vec<usize> = query
                .split("issue(number:")
                .skip(1)
                .map(|part| part.split(')').next().expect("number").parse().expect("numeric id"))
                .collect();
            assert!(!ids.is_empty() && ids.len() <= BOARD_BATCH_SIZE);
            self.calls.fetch_add(1, Ordering::SeqCst);
            // An external request takes time. Paused-clock deadlines cancel a
            // batch after earlier batches have already committed durably.
            tokio::time::sleep(Duration::from_millis(10)).await;
            let mut repository = serde_json::Map::new();
            let invalid = *self.invalid.lock().expect("invalid response");
            for id in ids {
                assert!(id > 0 && id <= self.count);
                let mut detail = json!({
                    "number":id,"title":format!("Issue {id}"),"state":"OPEN",
                    "url":format!("https://github.com/team/repo/issues/{id}"),
                    "updatedAt":"2026-10-08T00:00:00Z","closedAt":null,
                    "labels":{"nodes":[{"name":"ready"}],"pageInfo":{"hasNextPage":false}},
                    "blockedBy":{"totalCount":0,"nodes":[]},
                    "closedByPullRequestsReferences":{"nodes":[],"pageInfo":{"hasNextPage":false}},
                    "parent":null,"issueType":{"name":"Task"}
                });
                if let Some(field) = invalid {
                    detail[field]["pageInfo"]["hasNextPage"] = json!(true);
                }
                repository.insert(format!("issue{id}"), detail);
            }
            self.completed.fetch_add(1, Ordering::SeqCst);
            json!({"data":{"repository":repository,"rateLimit":{"cost":1,"remaining":self.remaining,
                "resetAt":(chrono::Utc::now()+chrono::Duration::hours(1)).to_rfc3339()}}})
        };
        Ok(CommandOutput { stdout: format!("HTTP/2 200 OK\r\n\r\n{body}"), stderr: String::new(), exit_code: Some(0) })
    }
    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}
fn source() -> IssueSource {
    IssueSource { service: "https://github.com".into(), scope: "team/repo".into() }
}
fn provider(forge: Arc<dyn CommandRunner>, directory: &Path) -> GitHubIssueProvider {
    GitHubIssueProvider::new(Arc::new(GhApiClient::new(forge.clone()).with_persistence(directory.join("rest"))), forge, Path::new("/"))
        .with_poll_directory(directory.into())
}

// #2928: deadlines shorter than a full inventory walk must converge after
// repeated refreshes, even with process restarts. Completed batches never repeat.
#[tokio::test(start_paused = true)]
async fn board_deadlines_and_restarts_converge_with_bounded_bulk_calls() {
    let forge = Arc::new(BoardForge::new(125));
    let directory = tempfile::tempdir().expect("durable cache");
    let budgets = crate::forge_budget::ForgeBudgets::default();
    let metered = Arc::new(crate::forge_budget::BudgetedRunner { inner: forge.clone(), budgets: budgets.clone() });
    let mut previous = 0;
    let mut timeouts = 0;
    loop {
        let provider = GitHubIssueProvider::new(Arc::new(GhApiClient::new(metered.clone())), metered.clone(), Path::new("/"))
            .with_poll_directory(directory.path().into());
        let result = tokio::time::timeout(Duration::from_millis(25), provider.dispatch_board(&source())).await;
        let state = provider.poll.load("team/repo").expect("stored progress");
        assert!(state.issues.details.len() > previous, "each interrupted refresh makes durable progress");
        previous = state.issues.details.len();
        if let Ok(board) = result {
            let board = board.expect("complete board");
            assert_eq!(board.issues.len(), 125);
            assert!(state.pending.is_none());
            break;
        }
        timeouts += 1;
        assert!(timeouts < 10, "refresh must converge");
    }
    assert!(timeouts > 0, "deadline must interrupt a full walk");
    assert_eq!(forge.completed.load(Ordering::SeqCst), 125_usize.div_ceil(BOARD_BATCH_SIZE));
    assert_eq!(forge.calls.load(Ordering::SeqCst), 125_usize.div_ceil(BOARD_BATCH_SIZE) + timeouts);
    let row = budgets.rows("test").into_iter().find(|row| row.budget == "GraphQL").expect("GraphQL attempts");
    assert_eq!(row.calls, forge.calls.load(Ordering::SeqCst) as u64);
    assert_eq!(row.reported_cost, forge.completed.load(Ordering::SeqCst) as u64);
    assert_eq!(row.unreported_calls, timeouts as u64);
}

// #2928: low budget reserves other users' points; the completed batch remains
// durable and retries (including a restarted provider) make no GraphQL calls.
// Generate empty, both sides of batch boundaries, and both sides of quota 100.
#[hegel::test]
fn board_bulk_boundaries_and_low_budget(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(45));
    let remaining = tc.draw(gs::integers::<u64>().min_value(98).max_value(101));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("runtime");
    runtime.block_on(async {
        let forge = Arc::new(BoardForge::builder().count(count).remaining(remaining).build());
        let directory = tempfile::tempdir().expect("cache");
        let first = provider(forge.clone(), directory.path());
        let result = first.dispatch_board(&source()).await;
        let expected_batches = if remaining < 100 { usize::from(count > 0) } else { count.div_ceil(BOARD_BATCH_SIZE) };
        assert_eq!(forge.calls.load(Ordering::SeqCst), expected_batches);
        let state = first.poll.load("team/repo").expect("durable batch");
        assert_eq!(state.issues.details.len(), if remaining < 100 { count.min(BOARD_BATCH_SIZE) } else { count });
        assert_eq!(result.is_ok(), remaining >= 100 || count <= BOARD_BATCH_SIZE);
        if count > 0 && remaining < 100 {
            let restarted = provider(forge.clone(), directory.path());
            assert!(restarted.dispatch_board(&source()).await.is_err());
            assert_eq!(forge.calls.load(Ordering::SeqCst), 1);
        }
    });
}

// Native windows must be complete before committing a batch. A refusal retains
// its inventory, so a corrected response recovers without recollecting revisions.
#[tokio::test(start_paused = true)]
async fn board_truncated_native_window_is_not_committed() {
    let forge = Arc::new(BoardForge::new(1));
    let directory = tempfile::tempdir().expect("cache");
    for field in ["labels", "closedByPullRequestsReferences"] {
        *forge.invalid.lock().expect("invalid response") = Some(field);
        let provider = provider(forge.clone(), directory.path());
        assert!(provider.dispatch_board(&source()).await.expect_err("truncated window").contains("truncated"));
        let state = provider.poll.load("team/repo").expect("state");
        assert!(state.issues.cursor.is_none());
        assert_eq!(state.pending.expect("pending retry").issues.len(), 1);
    }
    *forge.invalid.lock().expect("invalid response") = None;
    assert_eq!(provider(forge, directory.path()).dispatch_board(&source()).await.expect("recovered").issues.len(), 1);
}

// ADR 0047: forge-cache poll state is stored data. The previous generation's
// exact shape remains decodable; new progress fields default to no work/backoff.
#[test]
fn board_previous_generation_stored_shape_is_decodable() {
    let old = json!({
        "issues":{"cursor":null,"revisions":{},"details":{},"items":{},"check_revisions":{}},
        "pulls":{"cursor":null,"revisions":{},"details":{},"items":{},"check_revisions":{}}
    });
    let state: BoardState = serde_json::from_value(old).expect("previous-generation record");
    assert!(state.pending.is_none());
    assert!(state.retry_at.is_none());
    let encoded: Value = serde_json::to_value(state).expect("new stored record");
    assert!(encoded.get("pending").is_some());
}

// Scripted stand-in at the gh/HTTP boundary; unlike a permissive mock it
// refuses the wrong REST inventory state, delta cursor, or GraphQL subject set.
struct ScriptForge {
    requests: Mutex<std::collections::VecDeque<(String, String)>>,
    graphql_calls: AtomicUsize,
}
#[async_trait]
impl CommandRunner for ScriptForge {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        self.run_output(cmd, args, cwd, label).await.map(|output| output.stdout)
    }
    async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        assert_eq!(cmd, "gh");
        assert_eq!(args[0], "api");
        let (expected, response) = self.requests.lock().expect("requests").pop_front().expect("unexpected request");
        if args[1] == "graphql" {
            self.graphql_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(&args[..4], &["api", "graphql", "--include", "-f"]);
            let selections: Vec<_> = args[4]
                .split_whitespace()
                .filter(|part| part.starts_with("issue(number:") || part.starts_with("pullRequest(number:"))
                .collect();
            assert_eq!(selections.join(" "), expected);
            assert!(args[4].contains("rateLimit { cost remaining resetAt }"));
        } else {
            assert_eq!(args[1], "--include");
            assert_eq!(args[2], expected);
            if response.contains("304 Not Modified") {
                assert_eq!(args[3], "-H");
                assert!(args[4].starts_with("If-None-Match:"));
            }
        }
        Ok(CommandOutput { stdout: response, stderr: String::new(), exit_code: Some(0) })
    }
    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}
fn rest(etag: &str, value: Value) -> String {
    format!("HTTP/2 200 OK\r\nETag: {etag}\r\n\r\n{value}")
}
fn issue_detail(id: u64) -> Value {
    json!({"number":id,"title":format!("Issue {id}"),"state":"OPEN",
        "url":format!("https://github.com/team/repo/issues/{id}"),"updatedAt":"2026-10-08T00:00:00Z",
        "closedAt":null,"labels":{"nodes":[],"pageInfo":{"hasNextPage":false}},
        "blockedBy":{"totalCount":0,"nodes":[]},"closedByPullRequestsReferences":{"nodes":[],"pageInfo":{"hasNextPage":false}},
        "parent":null,"issueType":{"name":"Task"}})
}

// #2928: steady state fetches detail only for changed open revisions. Closing a
// blocker updates A's eligibility without fetching A again; cross-repo blockers
// use conditional REST checks. Only linked closed PR state is fetched.
#[tokio::test]
async fn board_incremental_closures_resolve_cached_blockers_and_linked_prs() {
    let issue_list = "repos/team/repo/issues?state=";
    let pull_list = "repos/team/repo/pulls?state=";
    let listing = |prefix: &str, state: &str| format!("{prefix}{state}&sort=updated&direction=desc&per_page=100&page=1");
    let a = json!({"number":7,"state":"open","updated_at":"2026-10-08T00:00:00Z"});
    let b = json!({"number":8,"state":"open","updated_at":"2026-10-08T00:00:00Z"});
    let pr = json!({"number":10,"state":"open","updated_at":"2026-10-08T00:00:00Z"});
    let closed_b = json!({"number":8,"state":"closed","updated_at":"2026-10-08T00:01:00Z"});
    let closed_pr = json!({"number":10,"state":"closed","updated_at":"2026-10-08T00:01:00Z"});
    let mut detail = issue_detail(7);
    detail["blockedBy"] = json!({"totalCount":2,"nodes":[
        {"url":"https://github.com/team/repo/issues/8","state":"OPEN"},
        {"url":"https://github.com/other/repo/issues/9","state":"OPEN"}
    ]});
    detail["closedByPullRequestsReferences"]["nodes"] = json!([{"url":"https://github.com/team/repo/pull/11"}]);
    let mut b_detail = issue_detail(8);
    b_detail["blockedBy"] = json!({"totalCount":1,"nodes":[{"url":"https://github.com/other/repo/issues/9","state":"OPEN"}]});
    let details = json!({"data":{"rateLimit":{"cost":2,"remaining":4000},"repository":{
        "issue7":detail,"issue8":b_detail,
        "pr10":{"number":10,"state":"OPEN","url":"https://github.com/team/repo/pull/10",
            "mergedAt":null,"mergeStateStatus":"CLEAN","commits":{"nodes":[{"commit":{"statusCheckRollup":{
                "contexts":{"nodes":[{"status":"COMPLETED","conclusion":"SUCCESS"}],"pageInfo":{"hasNextPage":false}}
            }}}]}}
    }}});
    let unchanged = "HTTP/2 304 Not Modified\r\n\r\n".to_string();
    let requests = vec![
        (listing(issue_list, "open"), rest("issues-open", json!([a.clone(), b]))),
        (listing(pull_list, "open"), rest("pulls-open", json!([pr.clone()]))),
        ("issue(number:7) issue(number:8) pullRequest(number:10)".into(), rest("graphql", details)),
        ("repos/other/repo/issues/9".into(), rest("blocker-open", json!({"state":"open"}))),
        ("repos/team/repo/pulls/11".into(), rest("linked", json!({"state":"closed","merged_at":"2026-10-08T00:00:00Z"}))),
        // A is unchanged; B and PR10 close. Their closed revisions require no GraphQL detail.
        (listing(issue_list, "all"), rest("issues-closed", json!([closed_b.clone(), a]))),
        (format!("{}&since=2026-10-07T23%3A59%3A59Z", listing(issue_list, "all")), rest("delta", json!([closed_b]))),
        (listing(pull_list, "all"), rest("pulls-closed", json!([closed_pr]))),
        ("repos/other/repo/issues/9".into(), rest("blocker-closed", json!({"state":"closed"}))),
        ("repos/team/repo/pulls/11".into(), unchanged.clone()),
        (listing(issue_list, "all"), unchanged.clone()),
        (listing(pull_list, "all"), unchanged.clone()),
        ("repos/other/repo/issues/9".into(), unchanged.clone()),
        ("repos/team/repo/pulls/11".into(), unchanged),
    ];
    let runner = Arc::new(ScriptForge { requests: Mutex::new(requests.into()), graphql_calls: AtomicUsize::new(0) });
    let budgets = crate::forge_budget::ForgeBudgets::default();
    let metered = Arc::new(crate::forge_budget::BudgetedRunner { inner: runner.clone(), budgets: budgets.clone() });
    let directory = tempfile::tempdir().expect("cache");
    let make = || {
        GitHubIssueProvider::new(
            Arc::new(GhApiClient::new(metered.clone()).with_persistence(directory.path().join("rest"))),
            metered.clone(),
            Path::new("/"),
        )
        .with_poll_directory(directory.path().join("board"))
    };
    let first = make().dispatch_board(&source()).await.expect("initial open inventory");
    assert_eq!(first.issues.len(), 2);
    assert!(first
        .issues
        .iter()
        .find(|issue| issue.id == "7")
        .expect("A")
        .blocked_by
        .iter()
        .all(|blocker| blocker.state == flotilla_protocol::IssueState::Open));
    assert_eq!(first.pull_requests.iter().find(|pr| pr.id == "10").expect("open PR").ci, "success");
    assert_eq!(first.pull_requests.iter().find(|pr| pr.id == "11").expect("linked closed PR").state, "merged");
    // Rediscovery/restart retains details and conditional HTTP bodies.
    let restarted = make();
    for _ in 0..2 {
        let board = restarted.dispatch_board(&source()).await.expect("incremental close/quiet pass");
        assert_eq!(board.issues.len(), 1);
        assert!(board.issues[0].blocked_by.iter().all(|blocker| blocker.state == flotilla_protocol::IssueState::Closed));
        assert_eq!(board.pull_requests.len(), 1, "unlinked closed PR10 is dropped");
        assert_eq!(board.pull_requests[0].id, "11");
        assert_eq!(runner.graphql_calls.load(Ordering::SeqCst), 1, "no timed re-read of unchanged open details");
    }
    let row = budgets.rows("test").into_iter().find(|row| row.budget == "GraphQL").expect("shared GraphQL budget");
    assert_eq!((row.calls, row.reported_cost), (1, 2));
    assert_eq!(row.remaining, Some(4000));
    assert!(runner.requests.lock().expect("requests").is_empty());
}

// Primary/secondary limits retain the unfinished inventory and a durable retry
// deadline. Restarted refreshes make no requests during the cooldown.
#[tokio::test]
async fn board_rate_limit_errors_back_off_across_restart() {
    for (remaining, message, retry_header) in [(0, "API rate limit exceeded", ""), (4000, "secondary rate limit", "Retry-After: 60\r\n")] {
        let reset = chrono::Utc::now() + chrono::Duration::hours(1);
        let raw = format!(
            "HTTP/2 200 OK\r\nX-RateLimit-Remaining: {remaining}\r\nX-RateLimit-Reset: {}\r\n{retry_header}\r\n{}",
            reset.timestamp(),
            json!({"errors":[{"type":"RATE_LIMITED","message":message}]})
        );
        let requests = vec![
            (
                "repos/team/repo/issues?state=open&sort=updated&direction=desc&per_page=100&page=1".into(),
                rest("issues", json!([{"number":1,"state":"open","updated_at":"2026-10-08T00:00:00Z"}])),
            ),
            ("repos/team/repo/pulls?state=open&sort=updated&direction=desc&per_page=100&page=1".into(), rest("pulls", json!([]))),
            ("issue(number:1)".into(), raw),
        ];
        let runner = Arc::new(ScriptForge { requests: Mutex::new(requests.into()), graphql_calls: AtomicUsize::new(0) });
        let directory = tempfile::tempdir().expect("cache");
        let make = || {
            GitHubIssueProvider::new(Arc::new(GhApiClient::new(runner.clone())), runner.clone(), Path::new("/"))
                .with_poll_directory(directory.path().into())
        };
        let first = make();
        let error = first.dispatch_board(&source()).await.expect_err("rate limited");
        let deadline = rate_limit_reset(&error).expect("compatible rate limit diagnostic");
        let mut state = first.poll.load("team/repo").expect("durable retry");
        assert_eq!(state.pending.as_ref().expect("unfinished inventory").issues.len(), 1);
        assert!(state.issues.details.is_empty());
        if remaining == 0 {
            assert_eq!(deadline.timestamp(), reset.timestamp());
        } else {
            assert!(deadline < chrono::Utc::now() + chrono::Duration::minutes(2), "secondary limit uses Retry-After, not hourly reset");
        }
        assert!(make().dispatch_board(&source()).await.is_err());
        assert_eq!(runner.graphql_calls.load(Ordering::SeqCst), 1);
        assert!(runner.requests.lock().expect("requests").is_empty());
        // Simulate expiry through the durable cache API; pending revisions must
        // resume without replaying the already collected REST inventory.
        state.retry_at = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
        first.poll.save("team/repo", &state).expect("expired retry deadline");
        runner.requests.lock().expect("requests").push_back((
            "issue(number:1)".into(),
            rest("recovered", json!({"data":{"rateLimit":{"cost":1,"remaining":4000},"repository":{"issue1":issue_detail(1)}}})),
        ));
        assert_eq!(make().dispatch_board(&source()).await.expect("recovered after deadline").issues.len(), 1);
        assert_eq!(runner.graphql_calls.load(Ordering::SeqCst), 2);
        assert!(runner.requests.lock().expect("requests").is_empty());
    }
}

// An empty initial open set still establishes an incremental cursor. Restarted
// quiet polls must not re-read state=open or walk old closed history; a later
// open issue is fetched once through the same delta/ETag path.
#[tokio::test]
async fn board_empty_inventory_enters_incremental_state_across_restart() {
    let listing = |kind: &str, state: &str| format!("repos/team/repo/{kind}?state={state}&sort=updated&direction=desc&per_page=100&page=1");
    let forge = Arc::new(ScriptForge {
        requests: Mutex::new(std::collections::VecDeque::from([
            (listing("issues", "open"), rest("empty-issues", json!([]))),
            (listing("pulls", "open"), rest("empty-pulls", json!([]))),
        ])),
        graphql_calls: AtomicUsize::new(0),
    });
    let directory = tempfile::tempdir().unwrap();
    let first = provider(forge.clone(), directory.path());
    assert!(first.dispatch_board(&source()).await.unwrap().issues.is_empty());
    let state = first.poll.load("team/repo").unwrap();
    let cursor: chrono::DateTime<chrono::Utc> = state.issues.cursor.as_ref().expect("empty issue cursor").parse().unwrap();
    assert_eq!(state.pulls.cursor, state.issues.cursor);
    let since =
        urlencoding::encode(&(cursor - chrono::Duration::seconds(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).into_owned();
    let old = json!({"number":1,"state":"closed","updated_at":(cursor - chrono::Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)});
    let old_page =
        rest("old-history", json!([old])).replace("HTTP/2 200 OK\r\n", "HTTP/2 200 OK\r\nLink: <old-history-page-2>; rel=\"next\"\r\n");
    forge.requests.lock().unwrap().extend([
        (listing("issues", "all"), old_page.clone()),
        (format!("{}&since={since}", listing("issues", "all")), old_page.clone()),
        (listing("pulls", "all"), old_page),
    ]);
    assert!(provider(forge.clone(), directory.path()).dispatch_board(&source()).await.unwrap().issues.is_empty());
    assert_eq!(forge.graphql_calls.load(Ordering::SeqCst), 0);
    let revision = (cursor + chrono::Duration::seconds(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let new = json!({"number":7,"state":"open","updated_at":revision});
    let mut detail = issue_detail(7);
    detail["updatedAt"] = json!(revision);
    forge.requests.lock().unwrap().extend([
        (listing("issues", "all"), rest("new-issue", json!([new.clone()]))),
        (format!("{}&since={since}", listing("issues", "all")), rest("new-delta", json!([new]))),
        (listing("pulls", "all"), "HTTP/2 304 Not Modified\r\n\r\n".into()),
        (
            "issue(number:7)".into(),
            rest("new-detail", json!({"data":{"rateLimit":{"cost":1,"remaining":4000},"repository":{"issue7":detail}}})),
        ),
    ]);
    let board = provider(forge.clone(), directory.path()).dispatch_board(&source()).await.unwrap();
    assert_eq!(board.issues.len(), 1);
    assert_eq!(board.issues[0].id, "7");
    assert_eq!(forge.graphql_calls.load(Ordering::SeqCst), 1);
    assert!(forge.requests.lock().unwrap().is_empty());
}
