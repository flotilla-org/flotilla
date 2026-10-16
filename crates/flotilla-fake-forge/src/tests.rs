use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_provider_runtime::{ChannelLabel, HttpClient};
use hegel::generators as gs;
use http::StatusCode;
use serde_json::{json, Value};

use crate::{
    model::{IssueFilter, PullFilter, StateFilter},
    CheckState, Clock, FakeForge, ForgeSettings, InProcessClient, NewIssue, NewPull, NewReview, RepoRef, ReviewState,
};

const PUBLIC_URL: &str = "https://fake-forge.test:3443";
const GITHUB_TOKEN: &str = "github-test-token";
const LAB_TOKEN: &str = "lab-test-token";

/// A clock tests move by hand.
struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 16, 12, 0, 0).single().expect("time"))))
    }

    fn advance(&self, by: Duration) {
        let mut now = self.0.lock().expect("clock");
        *now += chrono::Duration::from_std(by).expect("duration");
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().expect("clock")
    }
}

fn forge_with(clock: Arc<dyn Clock>) -> Arc<FakeForge> {
    let forge = Arc::new(FakeForge::new(PUBLIC_URL, clock));
    forge.add_forge("github", ForgeSettings { token: GITHUB_TOKEN.into(), login: "rehearsal".into() }).expect("github forge");
    forge.add_forge("lab", ForgeSettings { token: LAB_TOKEN.into(), login: "lab-bot".into() }).expect("lab forge");
    forge
}

fn forge() -> Arc<FakeForge> {
    forge_with(ManualClock::new())
}

fn repo() -> RepoRef {
    RepoRef::new("github", "flotilla-org", "flotilla")
}

struct Reply {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Value,
}

/// Sends one request through the provider-facing HTTP seam, with the headers
/// flotilla's Forgejo client sends unless the test overrides them.
struct Request {
    method: reqwest::Method,
    url: String,
    token: Option<String>,
    accept: Option<&'static str>,
    body: Option<Value>,
}

impl Request {
    fn new(method: reqwest::Method, path: &str) -> Self {
        Self { method, url: format!("{PUBLIC_URL}{path}"), token: Some(GITHUB_TOKEN.into()), accept: Some("application/json"), body: None }
    }

    fn get(path: &str) -> Self {
        Self::new(reqwest::Method::GET, path)
    }

    fn token(mut self, token: Option<&str>) -> Self {
        self.token = token.map(str::to_string);
        self
    }

    fn accept(mut self, accept: Option<&'static str>) -> Self {
        self.accept = accept;
        self
    }

    fn json(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    async fn send(self, forge: &Arc<FakeForge>) -> Reply {
        let mut request = reqwest::Request::new(self.method, self.url.parse().expect("URL"));
        let headers = request.headers_mut();
        if let Some(accept) = self.accept {
            headers.insert("accept", accept.parse().expect("header"));
        }
        if let Some(token) = self.token {
            headers.insert("authorization", format!("token {token}").parse().expect("header"));
        }
        if let Some(body) = self.body {
            headers.insert("content-type", "application/json".parse().expect("header"));
            *request.body_mut() = Some(serde_json::to_vec(&body).expect("JSON").into());
        }
        let response = InProcessClient::new(Arc::clone(forge)).execute(request, &ChannelLabel::Default).await.expect("delivered");
        let body = if response.body().is_empty() { Value::Null } else { serde_json::from_slice(response.body()).expect("JSON body") };
        Reply { status: response.status(), headers: response.headers().clone(), body }
    }
}

const PULLS: &str = "/github/api/v1/repos/flotilla-org/flotilla/pulls";
const ISSUES: &str = "/github/api/v1/repos/flotilla-org/flotilla/issues";

fn new_pull(head: &str) -> NewPull {
    NewPull { head: head.into(), base: "main".into(), title: format!("Work on {head}"), body: String::new(), user: None, draft: false }
}

fn new_issue(title: &str, labels: &[&str]) -> NewIssue {
    NewIssue { title: title.into(), labels: labels.iter().map(|label| label.to_string()).collect(), ..NewIssue::default() }
}

// Behaviour (#3374): pull requests and issues share one number sequence per
// repository, as on Forgejo, so hosts interleaving pull requests and issues
// through the API never see a number twice. Repositories number
// independently. The generator interleaves 1..=12 creations across two
// repositories, alternating between API-opened pull requests and admin-opened
// issues, so collisions between kinds and between repositories are both drawn.
#[hegel::test]
fn numbers_are_unique_per_repository_across_pulls_and_issues(tc: hegel::TestCase) {
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let operations: Vec<(bool, bool)> = (0..steps).map(|_| (tc.draw(gs::booleans()), tc.draw(gs::booleans()))).collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let forge = forge();
        let repos = [repo(), RepoRef::new("github", "flotilla-org", "other")];
        let mut expected = [0_u64, 0_u64];
        for (step, (second_repo, pull)) in operations.into_iter().enumerate() {
            let index = usize::from(second_repo);
            let target = &repos[index];
            let number = if pull {
                let path = format!("/github/api/v1/repos/{}/{}/pulls", target.owner, target.name);
                let reply = Request::new(reqwest::Method::POST, &path)
                    .json(json!({"head": format!("branch-{step}"), "base": "main", "title": format!("step {step}")}))
                    .send(&forge)
                    .await;
                assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
                reply.body["number"].as_u64().expect("number")
            } else {
                forge.create_issue(target, new_issue(&format!("step {step}"), &[])).expect("issue").number
            };
            expected[index] += 1;
            assert_eq!(number, expected[index], "each repository numbers from 1 without gaps or repeats");
        }
    });
}

// Behaviour: a request with the shape flotilla's providers send succeeds, and
// any request outside that shape is refused with a Forgejo error, logged and
// counted, never answered loosely. Each case changes one thing about a valid
// request: route, method, query parameter, value, header or body field.
#[tokio::test]
async fn requests_outside_the_provider_shapes_are_refused() {
    let forge = forge();
    forge.create_pull(&repo(), new_pull("feature")).expect("pull");
    let valid = Request::get(&format!("{PULLS}?state=open&limit=10&page=1")).send(&forge).await;
    assert_eq!(valid.status, StatusCode::OK);
    assert_eq!(forge.refusals().1, 0);

    let cases: Vec<(&str, Request, StatusCode)> = vec![
        ("unknown route", Request::get("/github/api/v1/repos/flotilla-org/flotilla/branches"), StatusCode::NOT_FOUND),
        (
            "unknown forge",
            Request::get("/nowhere/api/v1/repos/flotilla-org/flotilla/pulls?state=open&limit=1&page=1"),
            StatusCode::NOT_FOUND,
        ),
        ("no forge prefix", Request::get("/api/v1/repos/flotilla-org/flotilla/pulls?state=open&limit=1&page=1"), StatusCode::NOT_FOUND),
        ("unsupported method", Request::new(reqwest::Method::DELETE, &format!("{PULLS}/1")), StatusCode::METHOD_NOT_ALLOWED),
        ("unknown parameter", Request::get(&format!("{PULLS}?state=open&limit=10&page=1&sort=newest")), StatusCode::BAD_REQUEST),
        ("missing page", Request::get(&format!("{PULLS}?state=open&limit=10")), StatusCode::BAD_REQUEST),
        ("missing state", Request::get(&format!("{PULLS}?limit=10&page=1")), StatusCode::BAD_REQUEST),
        ("bad state", Request::get(&format!("{PULLS}?state=merged&limit=10&page=1")), StatusCode::BAD_REQUEST),
        ("limit above Forgejo's maximum", Request::get(&format!("{PULLS}?state=open&limit=51&page=1")), StatusCode::BAD_REQUEST),
        ("zero limit", Request::get(&format!("{PULLS}?state=open&limit=0&page=1")), StatusCode::BAD_REQUEST),
        ("zero page", Request::get(&format!("{PULLS}?state=open&limit=10&page=0")), StatusCode::BAD_REQUEST),
        ("repeated parameter", Request::get(&format!("{PULLS}?state=open&state=all&limit=10&page=1")), StatusCode::BAD_REQUEST),
        ("query on item read", Request::get(&format!("{PULLS}/1?fields=all")), StatusCode::BAD_REQUEST),
        ("non-numeric item", Request::get(&format!("{PULLS}/first")), StatusCode::NOT_FOUND),
        ("no token", Request::get(&format!("{PULLS}/1")).token(None), StatusCode::UNAUTHORIZED),
        ("wrong token", Request::get(&format!("{PULLS}/1")).token(Some("guess")), StatusCode::UNAUTHORIZED),
        ("another forge's token", Request::get(&format!("{PULLS}/1")).token(Some(LAB_TOKEN)), StatusCode::UNAUTHORIZED),
        ("no JSON accept", Request::get(&format!("{PULLS}/1")).accept(None), StatusCode::NOT_ACCEPTABLE),
        ("HTML accept", Request::get(&format!("{PULLS}/1")).accept(Some("text/html")), StatusCode::NOT_ACCEPTABLE),
        (
            "unknown edit field",
            Request::new(reqwest::Method::PATCH, &format!("{PULLS}/1")).json(json!({"body": "x", "milestone": 3})),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("empty edit", Request::new(reqwest::Method::PATCH, &format!("{PULLS}/1")).json(json!({})), StatusCode::UNPROCESSABLE_ENTITY),
        (
            "bad edit state",
            Request::new(reqwest::Method::PATCH, &format!("{PULLS}/1")).json(json!({"state": "merged"})),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("edit without body", Request::new(reqwest::Method::PATCH, &format!("{PULLS}/1")), StatusCode::UNSUPPORTED_MEDIA_TYPE),
        (
            "unknown merge style",
            Request::new(reqwest::Method::POST, &format!("{PULLS}/1/merge")).json(json!({"Do": "octopus"})),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "merge option in the wrong case",
            Request::new(reqwest::Method::POST, &format!("{PULLS}/1/merge")).json(json!({"do": "squash"})),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("reviews without page", Request::get(&format!("{PULLS}/1/reviews?limit=50")), StatusCode::BAD_REQUEST),
        ("issues without type", Request::get(&format!("{ISSUES}?state=open&sort=recentupdate&limit=30")), StatusCode::BAD_REQUEST),
        (
            "pull listing through issues",
            Request::get(&format!("{ISSUES}?state=open&type=pulls&sort=recentupdate&limit=30")),
            StatusCode::BAD_REQUEST,
        ),
        ("unserved sort", Request::get(&format!("{ISSUES}?state=open&type=issues&sort=oldest&limit=30")), StatusCode::BAD_REQUEST),
        (
            "issue match field Forgejo ignores",
            Request::get(&format!("{ISSUES}?state=open&type=issues&sort=recentupdate&limit=30&component=terminal")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "bad since",
            Request::get(&format!("{ISSUES}?state=all&type=issues&sort=recentupdate&limit=30&since=yesterday")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "empty label",
            Request::get(&format!("{ISSUES}?state=open&type=issues&sort=recentupdate&limit=30&labels=ready,")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "invalid repository name",
            Request::get("/github/api/v1/repos/flotilla-org/bad%20name/issues/1"),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ];
    let mut refused = 0;
    for (case, request, status) in cases {
        let reply = request.send(&forge).await;
        assert_eq!(reply.status, status, "{case}: {}", reply.body);
        assert!(reply.body["message"].as_str().is_some_and(|message| !message.is_empty()), "{case}: Forgejo error body");
        if status != StatusCode::UNPROCESSABLE_ENTITY || !case.contains("repository") {
            refused += 1;
            let (refusals, count) = forge.refusals();
            assert_eq!(count, refused, "{case}: counted as a refusal");
            assert_eq!(refusals.last().map(|refusal| refusal.status), Some(status.as_u16()), "{case}: logged");
        }
    }
    // None of the refused writes changed the pull request.
    let pull = forge.pull(&repo(), 1).expect("pull");
    assert_eq!((pull.body.as_str(), pull.merged()), ("", false));
}

// Behaviour: listings page the way the providers rely on. `X-Total-Count`
// counts every match, `Link` carries `rel="next"` exactly while a later page
// has items, and walking the pages yields every item once, newest first.
// The generator spans empty repositories, totals on and around page
// boundaries, and page sizes from 1 to Forgejo's maximum of 50.
#[hegel::test]
fn pagination_walks_every_pull_once(tc: hegel::TestCase) {
    let total = tc.draw(gs::integers::<usize>().min_value(0).max_value(60));
    let limit = tc.draw(gs::integers::<usize>().min_value(1).max_value(50));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let forge = forge();
        for index in 0..total {
            forge.create_pull(&repo(), new_pull(&format!("branch-{index}"))).expect("pull");
        }
        let mut seen = Vec::new();
        let mut page = 1;
        loop {
            let reply = Request::get(&format!("{PULLS}?state=all&limit={limit}&page={page}")).send(&forge).await;
            assert_eq!(reply.status, StatusCode::OK);
            assert_eq!(reply.headers["x-total-count"], total.to_string().as_str());
            let batch: Vec<u64> =
                reply.body.as_array().expect("array").iter().map(|pull| pull["number"].as_u64().expect("number")).collect();
            let has_next = reply.headers.get("link").and_then(|link| link.to_str().ok()).is_some_and(|link| link.contains("rel=\"next\""));
            assert_eq!(has_next, page * limit < total, "page {page} of {total} at {limit}");
            assert!(batch.len() <= limit);
            seen.extend(batch);
            if !has_next {
                break;
            }
            page += 1;
        }
        let expected: Vec<u64> = (1..=total as u64).rev().collect();
        assert_eq!(seen, expected);
        // A page past the end is empty rather than an error, as on Forgejo.
        let past = Request::get(&format!("{PULLS}?state=all&limit={limit}&page={}", page + 1)).send(&forge).await;
        assert_eq!((past.status, past.body.clone()), (StatusCode::OK, json!([])));
    });
}

// Behaviour (recorded lab evidence, forgejo_head_filter_capability.yaml):
// `head` matches an exact live branch name; an `owner:branch` form and a
// deleted head's synthetic `refs/pull/<n>/head` match nothing, while the
// unfiltered listing still reports the deleted head under that ref.
#[tokio::test]
async fn head_filter_matches_live_branches_only() {
    let forge = forge();
    let first = forge.create_pull(&repo(), new_pull("old-work")).expect("first");
    forge.merge(&repo(), first.number, "merge", None).expect("merge");
    forge.delete_head(&repo(), first.number).expect("delete head");
    let second = forge.create_pull(&repo(), new_pull("drop-retired-stance")).expect("second");
    let heads = |reply: &Reply| -> Vec<u64> {
        reply.body.as_array().expect("array").iter().map(|pull| pull["number"].as_u64().expect("n")).collect()
    };
    for (head, expected) in [
        ("drop-retired-stance", vec![second.number]),
        ("rehearsal%3Adrop-retired-stance", vec![]),
        ("refs%2Fpull%2F1%2Fhead", vec![]),
        ("old-work", vec![]),
        ("absent", vec![]),
    ] {
        let reply = Request::get(&format!("{PULLS}?state=all&limit=50&page=1&head={head}")).send(&forge).await;
        assert_eq!(heads(&reply), expected, "head={head}");
    }
    let all = Request::get(&format!("{PULLS}?state=all&limit=50&page=1")).send(&forge).await;
    assert_eq!(all.body[1]["head"]["ref"], "refs/pull/1/head");
    assert_eq!(all.body[1]["merged"], true);
}

// Behaviour: the pull request lifecycle the change-request provider drives.
// Opening returns 201 with the token owner as author; a second open pull
// request for the same head and base conflicts (409); an edit answers 201;
// merging an open pull request closes it as merged; merging again or
// reopening a merged one is refused with Forgejo's 405; a closed pull request
// cannot merge.
#[tokio::test]
async fn pull_request_lifecycle_follows_forgejo() {
    let forge = forge();
    let opened = Request::new(reqwest::Method::POST, PULLS)
        .json(json!({"head": "feature", "base": "main", "title": "Feature", "body": "Notes"}))
        .send(&forge)
        .await;
    assert_eq!(opened.status, StatusCode::CREATED);
    assert_eq!(opened.body["user"]["login"], "rehearsal");
    assert_eq!(opened.body["html_url"], format!("{PUBLIC_URL}/github/flotilla-org/flotilla/pulls/1"));
    assert_eq!((opened.body["state"].as_str(), opened.body["merged"].as_bool()), (Some("open"), Some(false)));
    let duplicate =
        Request::new(reqwest::Method::POST, PULLS).json(json!({"head": "feature", "base": "main", "title": "Again"})).send(&forge).await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);

    let edited = Request::new(reqwest::Method::PATCH, &format!("{PULLS}/1")).json(json!({"body": "Notes\n\nFixes #4"})).send(&forge).await;
    assert_eq!((edited.status, edited.body["body"].as_str()), (StatusCode::CREATED, Some("Notes\n\nFixes #4")));

    let merged = Request::new(reqwest::Method::POST, &format!("{PULLS}/1/merge")).json(json!({"Do": "squash"})).send(&forge).await;
    assert_eq!((merged.status, merged.body.clone()), (StatusCode::OK, Value::Null));
    let read = Request::get(&format!("{PULLS}/1")).send(&forge).await;
    assert_eq!((read.body["state"].as_str(), read.body["merged"].as_bool()), (Some("closed"), Some(true)));
    assert!(read.body["merged_at"].is_string());
    let again = Request::new(reqwest::Method::POST, &format!("{PULLS}/1/merge")).json(json!({"Do": "squash"})).send(&forge).await;
    assert_eq!(again.status, StatusCode::METHOD_NOT_ALLOWED);
    let reopen = Request::new(reqwest::Method::PATCH, &format!("{PULLS}/1")).json(json!({"state": "open"})).send(&forge).await;
    assert_eq!(reopen.status, StatusCode::METHOD_NOT_ALLOWED);

    // After the merge the same head may open a new pull request.
    let next = forge.create_pull(&repo(), new_pull("feature")).expect("reopened work");
    let closed =
        Request::new(reqwest::Method::PATCH, &format!("{PULLS}/{}", next.number)).json(json!({"state": "closed"})).send(&forge).await;
    assert_eq!(closed.body["state"], "closed");
    let refused =
        Request::new(reqwest::Method::POST, &format!("{PULLS}/{}/merge", next.number)).json(json!({"Do": "merge"})).send(&forge).await;
    assert_eq!(refused.status, StatusCode::METHOD_NOT_ALLOWED);
    // Domain answers are not contract refusals.
    assert_eq!(forge.refusals().1, 0);
    let missing = Request::get(&format!("{PULLS}/99")).send(&forge).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(forge.refusals().1, 0);
}

// Behaviour: reviews list oldest first with `X-Total-Count`, carrying the
// reviewer, state, dismissal and staleness the review-decision reader uses.
// A new head commit makes earlier reviews stale.
#[tokio::test]
async fn reviews_report_state_dismissal_and_staleness() {
    let forge = forge();
    forge.create_pull(&repo(), new_pull("feature")).expect("pull");
    let review = |user: &str, state| NewReview { user: user.into(), state, body: String::new() };
    let changes = forge.add_review(&repo(), 1, review("alice", ReviewState::RequestChanges)).expect("review");
    forge.add_review(&repo(), 1, review("bob", ReviewState::Approved)).expect("review");
    forge.dismiss_review(&repo(), 1, changes.id).expect("dismiss");
    forge.push_head(&repo(), 1).expect("push");
    forge.add_review(&repo(), 1, review("carol", ReviewState::Comment)).expect("review");

    let reply = Request::get(&format!("{PULLS}/1/reviews?limit=2&page=1")).send(&forge).await;
    assert_eq!(reply.headers["x-total-count"], "3");
    let page: Vec<(String, String, bool, bool)> = reply
        .body
        .as_array()
        .expect("array")
        .iter()
        .map(|review| {
            (
                review["user"]["login"].as_str().expect("login").to_string(),
                review["state"].as_str().expect("state").to_string(),
                review["dismissed"].as_bool().expect("dismissed"),
                review["stale"].as_bool().expect("stale"),
            )
        })
        .collect();
    assert_eq!(page, vec![("alice".into(), "REQUEST_CHANGES".into(), true, true), ("bob".into(), "APPROVED".into(), false, true)]);
    let last = Request::get(&format!("{PULLS}/1/reviews?limit=2&page=2")).send(&forge).await;
    assert_eq!(last.body[0]["state"], "COMMENT");
    assert_eq!(last.body[0]["stale"], false);
    let head = Request::get(&format!("{PULLS}/1")).send(&forge).await;
    assert_eq!(last.body[0]["commit_id"], head.body["head"]["sha"]);
}

// Behaviour: the issue listing serves flotilla's two queries. The open query
// returns only open issues (never pull requests), most recently updated
// first, filtered by every named label and by a case-insensitive search of
// title and body. The changed-since query includes closed issues updated at or
// after `since`. Reading a pull request's number through the issue endpoint
// returns it with its `pull_request` summary, as on Forgejo.
#[tokio::test]
async fn issue_queries_filter_order_and_include_closed_changes() {
    let clock = ManualClock::new();
    let forge = forge_with(clock.clone());
    let old = forge.create_issue(&repo(), new_issue("Old ready work", &["ready"])).expect("issue");
    clock.advance(Duration::from_secs(60));
    let since = clock.now();
    forge.create_pull(&repo(), new_pull("feature")).expect("pull");
    let both = forge.create_issue(&repo(), new_issue("Anchor terminal tabs", &["ready", "terminal"])).expect("issue");
    let closed = forge.create_issue(&repo(), new_issue("Closed anchor", &["ready"])).expect("issue");
    forge
        .edit(&repo(), closed.number, crate::ItemEdit { state: Some(crate::ItemState::Closed), ..crate::ItemEdit::default() })
        .expect("close");
    forge.add_comment(&repo(), old.number, "alice", "still relevant").expect("comment");

    let numbers = |reply: &Reply| -> Vec<u64> {
        reply.body.as_array().expect("array").iter().map(|issue| issue["number"].as_u64().expect("n")).collect()
    };
    let open = Request::get(&format!("{ISSUES}?state=open&type=issues&sort=recentupdate&page=1&limit=30")).send(&forge).await;
    assert_eq!(numbers(&open), vec![old.number, both.number], "the comment made the old issue the most recently updated");
    assert!(open.body.as_array().expect("array").iter().all(|issue| issue["pull_request"].is_null()));
    let labelled = Request::get(&format!("{ISSUES}?state=open&type=issues&sort=recentupdate&page=1&limit=30&labels=ready%2Cterminal"))
        .send(&forge)
        .await;
    assert_eq!(numbers(&labelled), vec![both.number]);
    let searched = Request::get(&format!("{ISSUES}?state=open&type=issues&sort=recentupdate&page=1&limit=30&q=ANCHOR")).send(&forge).await;
    assert_eq!(numbers(&searched), vec![both.number]);
    let since_text = crate::render::time(since).replace(':', "%3A");
    let changed = Request::get(&format!("{ISSUES}?state=all&type=issues&since={since_text}&sort=recentupdate&limit=30")).send(&forge).await;
    assert_eq!(numbers(&changed), vec![old.number, closed.number, both.number]);
    assert_eq!(changed.body[1]["state"], "closed");

    let pull_as_issue = Request::get(&format!("{ISSUES}/2")).send(&forge).await;
    assert_eq!(pull_as_issue.body["pull_request"]["merged"], false);
    let issue = Request::get(&format!("{ISSUES}/{}", both.number)).send(&forge).await;
    let labels: Vec<&str> =
        issue.body["labels"].as_array().expect("labels").iter().map(|label| label["name"].as_str().expect("name")).collect();
    assert_eq!(labels, vec!["ready", "terminal"]);
}

// Behaviour: each forge is a separate namespace with its own token. The same
// repository name on two forges holds separate items, and a forge's token
// reaches only that forge.
#[tokio::test]
async fn forges_are_separate_namespaces() {
    let forge = forge();
    forge.create_pull(&repo(), new_pull("feature")).expect("github pull");
    let lab_path = "/lab/api/v1/repos/flotilla-org/flotilla/pulls?state=all&limit=50&page=1";
    let lab = Request::get(lab_path).token(Some(LAB_TOKEN)).send(&forge).await;
    assert_eq!((lab.status, lab.body.clone()), (StatusCode::OK, json!([])));
    let crossed = Request::get(lab_path).send(&forge).await;
    assert_eq!(crossed.status, StatusCode::UNAUTHORIZED);
    let user = Request::get("/lab/api/v1/user").token(Some(LAB_TOKEN)).send(&forge).await;
    assert_eq!(user.body["login"], "lab-bot");
}

// Behaviour: the in-process client reaches only the fake's origin; a request
// for a real forge fails rather than leaving the process.
#[tokio::test]
async fn the_client_reaches_only_the_fake() {
    let forge = forge();
    let request =
        reqwest::Request::new(reqwest::Method::GET, "https://api.github.com/repos/flotilla-org/flotilla/pulls".parse().expect("URL"));
    let error = InProcessClient::new(forge).execute(request, &ChannelLabel::Default).await.expect_err("unreachable");
    assert!(error.contains("not reachable"), "{error}");
}

// Behaviour: with auto-merge, an open, ready pull request merges once it has
// been open for the delay; drafts, closed pull requests and younger ones stay.
#[test]
fn merge_due_merges_ready_pulls_after_the_delay() {
    let clock = ManualClock::new();
    let forge = forge_with(clock.clone());
    let ready = forge.create_pull(&repo(), new_pull("ready")).expect("pull");
    let draft = forge.create_pull(&repo(), NewPull { draft: true, ..new_pull("draft") }).expect("pull");
    let closed = forge.create_pull(&repo(), new_pull("closed")).expect("pull");
    forge
        .edit(&repo(), closed.number, crate::ItemEdit { state: Some(crate::ItemState::Closed), ..crate::ItemEdit::default() })
        .expect("close");
    clock.advance(Duration::from_secs(119));
    assert!(forge.merge_due(Duration::from_secs(120)).is_empty());
    let young = forge.create_pull(&repo(), new_pull("young")).expect("pull");
    clock.advance(Duration::from_secs(1));
    assert_eq!(forge.merge_due(Duration::from_secs(120)), vec![(repo(), ready.number)]);
    for number in [draft.number, closed.number, young.number] {
        assert!(!forge.pull(&repo(), number).expect("pull").merged(), "#{number} stays unmerged");
    }
    forge.set_checks(&repo(), young.number, CheckState::Success).expect("checks");
    assert_eq!(forge.pull(&repo(), young.number).expect("pull").pull.expect("facts").checks, Some(CheckState::Success));
}

// Behaviour: the model-level filters agree with the API's (glue for the
// admin layer, which calls the model directly).
#[test]
fn model_filters_select_by_state() {
    let forge = forge();
    forge.create_pull(&repo(), new_pull("a")).expect("pull");
    let page = forge.list_pulls(&repo(), &PullFilter { state: StateFilter::Closed, head: None }, 1, 10).expect("list");
    assert_eq!(page.total, 0);
    let issues = forge
        .list_issues(&repo(), &IssueFilter { state: StateFilter::All, labels: vec![], query: None, since: None }, 1, 10)
        .expect("issues");
    assert_eq!(issues.total, 0, "pull requests are not issues");
}
