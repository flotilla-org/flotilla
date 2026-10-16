use std::sync::{Arc, Mutex};

use axum::{
    body::{to_bytes, Body},
    Router,
};
use chrono::{DateTime, TimeZone, Utc};
use hegel::generators as gs;
use http::{Method, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{api, model::Clock, FakeForge, ForgeSettings};

const PUBLIC_URL: &str = "https://fake-forge.test:3443";
const ADMIN_TOKEN: &str = "admin-test-token";
const GITHUB_TOKEN: &str = "github-test-token";
const REPO: &str = "/admin/v1/forges/github/repos/flotilla-org/flotilla";
const API: &str = "/github/api/v1/repos/flotilla-org/flotilla";

struct FixedClock(Mutex<DateTime<Utc>>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().expect("clock")
    }
}

struct Reply {
    status: StatusCode,
    body: Value,
}

/// The two routers over one forge, driven without sockets.
struct Harness {
    forge: Arc<FakeForge>,
    api: Router,
    admin: Router,
}

impl Harness {
    fn new() -> Self {
        let clock = Arc::new(FixedClock(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 16, 12, 0, 0).single().expect("time"))));
        let forge = Arc::new(FakeForge::new(PUBLIC_URL, clock));
        forge.add_forge("github", ForgeSettings { token: GITHUB_TOKEN.into(), login: "rehearsal".into() }).expect("forge");
        Self { api: api::router(Arc::clone(&forge)), admin: super::router(Arc::clone(&forge), ADMIN_TOKEN), forge }
    }

    async fn send(router: &Router, request: http::Request<Body>) -> Reply {
        let response = router.clone().oneshot(request).await.expect("infallible");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("body");
        let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).expect("JSON body") };
        Reply { status, body }
    }

    async fn admin_as(&self, authorization: Option<&str>, method: Method, path: &str, body: Option<Value>) -> Reply {
        let mut request = http::Request::builder().method(method).uri(path);
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        let request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(serde_json::to_vec(&body).expect("JSON"))),
            None => request.body(Body::empty()),
        };
        Self::send(&self.admin, request.expect("request")).await
    }

    async fn admin(&self, method: Method, path: &str, body: Option<Value>) -> Reply {
        self.admin_as(Some(&format!("Bearer {ADMIN_TOKEN}")), method, path, body).await
    }

    async fn post(&self, path: &str, body: Option<Value>) -> Reply {
        self.admin(Method::POST, path, body).await
    }

    /// A Forgejo API read, as flotilla's providers send it.
    async fn api_get(&self, path: &str) -> Reply {
        let request = http::Request::get(format!("{API}{path}"))
            .header("authorization", format!("token {GITHUB_TOKEN}"))
            .header("accept", "application/json")
            .body(Body::empty())
            .expect("request");
        let reply = Self::send(&self.api, request).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}: {}", reply.body);
        reply
    }

    async fn open_pull(&self, head: &str) -> u64 {
        let reply =
            self.post(&format!("{REPO}/pulls"), Some(json!({"head": head, "base": "main", "title": format!("Work on {head}")}))).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.body["number"].as_u64().expect("number")
    }
}

fn numbers(reply: &Reply) -> Vec<u64> {
    reply.body.as_array().expect("array").iter().map(|item| item["number"].as_u64().expect("number")).collect()
}

// Behaviour (#3374): every admin request needs `Authorization: Bearer <admin
// token>`. A missing header, a wrong token, the Forgejo `token` scheme and a
// forge's own API token are all refused with 401, counted as refusals, and
// change nothing, whichever route they name.
#[tokio::test]
async fn admin_requests_need_the_admin_bearer_token() {
    let harness = Harness::new();
    let number = harness.open_pull("feature").await;
    let merge = format!("{REPO}/pulls/{number}/merge");
    for (case, authorization) in [
        ("missing", None),
        ("wrong token", Some("Bearer guess".to_string())),
        ("token scheme", Some(format!("token {ADMIN_TOKEN}"))),
        ("forge token", Some(format!("Bearer {GITHUB_TOKEN}"))),
        ("lower-case scheme", Some(format!("bearer {ADMIN_TOKEN}"))),
    ] {
        let before = harness.forge.refusals().1;
        for (method, path) in [(Method::POST, merge.as_str()), (Method::GET, "/admin/v1/state"), (Method::GET, "/admin/v1/nowhere")] {
            let reply = harness.admin_as(authorization.as_deref(), method, path, None).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{case} {path}: {}", reply.body);
        }
        assert_eq!(harness.forge.refusals().1, before + 3, "{case}: each refusal is counted");
    }
    let pull = harness.api_get(&format!("/pulls/{number}")).await;
    assert_eq!(pull.body["merged"], false, "no unauthenticated request merged the pull");
}

// Behaviour: requests outside the admin routes' shapes are refused with a
// Forgejo-style error and counted: unknown routes, wrong methods, unknown body
// fields, bad enum values, bodies on bodyless routes, missing required bodies,
// a non-JSON content type and query parameters.
#[tokio::test]
async fn admin_requests_outside_the_route_shapes_are_refused() {
    let harness = Harness::new();
    let number = harness.open_pull("feature").await;
    let pull = format!("{REPO}/pulls/{number}");
    let cases: Vec<(&str, Method, String, Option<Value>, StatusCode)> = vec![
        ("unknown route", Method::POST, format!("{pull}/rebase"), None, StatusCode::NOT_FOUND),
        ("outside the prefix", Method::GET, "/state".into(), None, StatusCode::NOT_FOUND),
        ("wrong method", Method::POST, format!("{pull}/checks"), Some(json!({"state": "success"})), StatusCode::METHOD_NOT_ALLOWED),
        ("state is read-only", Method::POST, "/admin/v1/state".into(), None, StatusCode::METHOD_NOT_ALLOWED),
        (
            "unknown field",
            Method::POST,
            format!("{pull}/merge"),
            Some(json!({"style": "squash", "force": true})),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("unknown merge style", Method::POST, format!("{pull}/merge"), Some(json!({"style": "octopus"})), StatusCode::UNPROCESSABLE_ENTITY),
        ("unknown check state", Method::PUT, format!("{pull}/checks"), Some(json!({"state": "green"})), StatusCode::UNPROCESSABLE_ENTITY),
        (
            "review state case",
            Method::POST,
            format!("{pull}/reviews"),
            Some(json!({"user": "a", "state": "approved"})),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("body on a bodyless route", Method::POST, format!("{pull}/close"), Some(json!({})), StatusCode::UNPROCESSABLE_ENTITY),
        ("missing body", Method::POST, format!("{REPO}/issues"), None, StatusCode::UNPROCESSABLE_ENTITY),
        (
            "comma in a label",
            Method::PUT,
            format!("{REPO}/issues/{number}/labels"),
            Some(json!({"labels": ["a,b"]})),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("query parameter", Method::GET, "/admin/v1/state?verbose=1".into(), None, StatusCode::BAD_REQUEST),
        ("non-numeric pull", Method::POST, format!("{REPO}/pulls/first/merge"), None, StatusCode::NOT_FOUND),
        (
            "unknown forge",
            Method::POST,
            "/admin/v1/forges/nowhere/repos/a/b/issues".into(),
            Some(json!({"title": "t"})),
            StatusCode::NOT_FOUND,
        ),
    ];
    for (case, method, path, body, status) in cases {
        let before = harness.forge.refusals().1;
        let reply = harness.admin(method, &path, body).await;
        assert_eq!(reply.status, status, "{case}: {}", reply.body);
        assert!(reply.body["message"].as_str().is_some_and(|message| !message.is_empty()), "{case}: error body");
        assert_eq!(harness.forge.refusals().1, before + 1, "{case}: counted as a refusal");
    }
    let text = http::Request::post(format!("{pull}/merge"))
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header("content-type", "text/plain")
        .body(Body::from("{}"))
        .expect("request");
    assert_eq!(Harness::send(&harness.admin, text).await.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(harness.api_get(&format!("/pulls/{number}")).await.body["merged"], false);
    let refusals = harness.admin(Method::GET, "/admin/v1/refusals", None).await;
    assert_eq!(refusals.body["total"], harness.forge.refusals().1);
    assert_eq!(refusals.body["refusals"][0]["path"], format!("{pull}/rebase"));
}

// Behaviour: each scenario control changes what the Forgejo API then reports
// to the providers. Admin-opened pull requests list through the API; a merge
// shows as merged and closed with its merger; reviews list with their state,
// and a pushed head makes them stale; deleting the head reports the pull ref;
// draft and ready toggle `draft`; requested reviewers and comments appear; an
// admin issue with labels is found by the label filter, and relabelling,
// closing and reopening it show in the issue listing.
#[tokio::test]
async fn scenario_controls_show_through_the_forgejo_api() {
    let harness = Harness::new();
    let ensured = harness.post(REPO, Some(json!({"default_branch": "trunk"}))).await;
    assert_eq!((ensured.status, ensured.body["default_branch"].as_str()), (StatusCode::OK, Some("trunk")));

    let opened = harness
        .post(
            &format!("{REPO}/pulls"),
            Some(json!({"head": "feature", "base": "trunk", "title": "Feature", "user": "alice", "draft": true})),
        )
        .await;
    assert_eq!(opened.status, StatusCode::CREATED);
    let number = opened.body["number"].as_u64().expect("number");
    let listed = harness.api_get("/pulls?state=open&limit=50&page=1").await;
    assert_eq!(numbers(&listed), vec![number]);
    assert_eq!((listed.body[0]["user"]["login"].as_str(), listed.body[0]["draft"].as_bool()), (Some("alice"), Some(true)));
    assert_eq!(listed.body[0]["base"]["repo"]["default_branch"], "trunk");

    let pull = format!("{REPO}/pulls/{number}");
    assert_eq!(harness.post(&format!("{pull}/merge"), None).await.status, StatusCode::METHOD_NOT_ALLOWED, "a draft cannot merge");
    assert_eq!(harness.post(&format!("{pull}/ready"), None).await.body["pull"]["draft"], false);
    assert_eq!(harness.api_get(&format!("/pulls/{number}")).await.body["draft"], false);
    harness.post(&format!("{pull}/draft"), None).await;
    assert_eq!(harness.api_get(&format!("/pulls/{number}")).await.body["draft"], true);
    harness.post(&format!("{pull}/ready"), None).await;

    let review =
        harness.post(&format!("{pull}/reviews"), Some(json!({"user": "bob", "state": "REQUEST_CHANGES", "body": "Needs work"}))).await;
    assert_eq!(review.status, StatusCode::CREATED);
    let review_id = review.body["id"].as_u64().expect("review id");
    harness.post(&format!("{pull}/reviews"), Some(json!({"user": "carol", "state": "APPROVED"}))).await;
    let dismissed = harness.post(&format!("{pull}/reviews/{review_id}/dismiss"), None).await;
    assert_eq!((dismissed.status, dismissed.body["dismissed"].as_bool()), (StatusCode::OK, Some(true)));
    let reviews = harness.api_get(&format!("/pulls/{number}/reviews?limit=50&page=1")).await;
    let states: Vec<(&str, &str, bool, bool)> = reviews
        .body
        .as_array()
        .expect("reviews")
        .iter()
        .map(|review| {
            (
                review["user"]["login"].as_str().expect("login"),
                review["state"].as_str().expect("state"),
                review["dismissed"].as_bool().expect("dismissed"),
                review["stale"].as_bool().expect("stale"),
            )
        })
        .collect();
    assert_eq!(states, vec![("bob", "REQUEST_CHANGES", true, false), ("carol", "APPROVED", false, false)]);
    let before_push = harness.api_get(&format!("/pulls/{number}")).await.body["head"]["sha"].clone();
    harness.post(&format!("{pull}/push"), None).await;
    let after_push = harness.api_get(&format!("/pulls/{number}")).await;
    assert_ne!(after_push.body["head"]["sha"], before_push);
    let reviews = harness.api_get(&format!("/pulls/{number}/reviews?limit=50&page=1")).await;
    assert!(reviews.body.as_array().expect("reviews").iter().all(|review| review["stale"] == true), "a push makes reviews stale");

    let checks = harness.admin(Method::PUT, &format!("{pull}/checks"), Some(json!({"state": "failure"}))).await;
    assert_eq!(checks.body["pull"]["checks"], "failure");
    let state = harness.admin(Method::GET, "/admin/v1/state", None).await;
    assert_eq!(state.body["github"]["repositories"]["flotilla-org/flotilla"]["items"][number.to_string()]["pull"]["checks"], "failure");
    assert_eq!(state.body["github"]["settings"]["token"], "<redacted>", "the state never shows tokens");

    harness.post(&format!("{pull}/requested-reviewers"), Some(json!({"login": "dave"}))).await;
    let comment = harness.post(&format!("{REPO}/issues/{number}/comments"), Some(json!({"user": "erin", "body": "LGTM"}))).await;
    assert_eq!(comment.status, StatusCode::CREATED);
    let read = harness.api_get(&format!("/pulls/{number}")).await;
    assert_eq!(read.body["requested_reviewers"][0]["login"], "dave");
    assert_eq!(read.body["comments"], 1);

    let merged = harness.post(&format!("{pull}/merge"), Some(json!({"style": "squash", "user": "maintainer"}))).await;
    assert_eq!(merged.status, StatusCode::OK);
    let read = harness.api_get(&format!("/pulls/{number}")).await;
    assert_eq!((read.body["state"].as_str(), read.body["merged"].as_bool()), (Some("closed"), Some(true)));
    assert_eq!(read.body["merged_by"]["login"], "maintainer");
    assert_eq!(harness.post(&format!("{pull}/reopen"), None).await.status, StatusCode::METHOD_NOT_ALLOWED);
    harness.post(&format!("{pull}/delete-head"), None).await;
    let read = harness.api_get(&format!("/pulls/{number}")).await;
    assert_eq!(read.body["head"]["ref"], format!("refs/pull/{number}/head"));

    let issue = harness
        .post(&format!("{REPO}/issues"), Some(json!({"title": "Ready work", "labels": ["ready", "terminal"], "assignees": ["frank"]})))
        .await;
    assert_eq!(issue.status, StatusCode::CREATED);
    let issue_number = issue.body["number"].as_u64().expect("number");
    assert_eq!(issue_number, number + 1, "issues and pulls share one number sequence");
    harness.post(&format!("{REPO}/issues"), Some(json!({"title": "Unlabelled"}))).await;
    let ready = "/issues?state=open&type=issues&sort=recentupdate&limit=50&labels=ready";
    assert_eq!(numbers(&harness.api_get(ready).await), vec![issue_number]);
    let relabelled =
        harness.admin(Method::PUT, &format!("{REPO}/issues/{issue_number}/labels"), Some(json!({"labels": ["blocked"]}))).await;
    assert_eq!(relabelled.status, StatusCode::OK);
    assert_eq!(numbers(&harness.api_get(ready).await), Vec::<u64>::new());
    harness.post(&format!("{REPO}/issues/{issue_number}/close"), None).await;
    let closed = harness.api_get(&format!("/issues/{issue_number}")).await;
    assert_eq!(closed.body["state"], "closed");
    harness.post(&format!("{REPO}/issues/{issue_number}/reopen"), None).await;
    assert_eq!(harness.api_get(&format!("/issues/{issue_number}")).await.body["state"], "open");
    assert_eq!(harness.forge.refusals().1, 0, "every control above is within its route's shape");
}

/// One admin operation the property draws. Targets index into the items
/// opened so far.
#[derive(Debug, Clone, Copy)]
enum Step {
    OpenPull { draft: bool },
    OpenIssue { ready: bool },
    Merge(usize),
    Close(usize),
    Reopen(usize),
    Ready(usize),
    Relabel(usize, bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Expected {
    pull: bool,
    open: bool,
    merged: bool,
    draft: bool,
    ready: bool,
}

fn draw_step(tc: &hegel::TestCase) -> Step {
    let target = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(6)) {
        0 => Step::OpenPull { draft: tc.draw(gs::booleans()) },
        1 => Step::OpenIssue { ready: tc.draw(gs::booleans()) },
        2 => Step::Merge(target),
        3 => Step::Close(target),
        4 => Step::Reopen(target),
        5 => Step::Ready(target),
        _ => Step::Relabel(target, tc.draw(gs::booleans())),
    }
}

// Behaviour (#3374), as a stateful property: any sequence of admin operations
// leaves the Forgejo API reporting a consistent forge. After every step:
// numbers are unique across pull requests and issues and run 1..=n; every
// item reports the state a reference model predicts; a merged pull request
// is closed; the `ready` label filter returns exactly the open issues
// carrying it. Each admin answer matches the model too: merging needs an
// open, non-draft pull request, and reopening a merged one is refused (405).
// The generator draws 1..=16 steps over every operation kind, with targets
// that hit pulls, issues and numbers not yet opened (404), so closes,
// reopens and merges land on every state.
//
// A case runs in milliseconds, so the property affords more cases than the
// default profile to reach multi-step sequences such as open, merge, reopen.
#[test]
fn admin_operations_keep_the_api_view_consistent() {
    hegel::Hegel::new(admin_operations_case).settings(hegel::Settings::new().test_cases(200)).run();
}

fn admin_operations_case(tc: hegel::TestCase) {
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(16));
    let steps: Vec<Step> = (0..count).map(|_| draw_step(&tc)).collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let harness = Harness::new();
        let mut model: Vec<Expected> = Vec::new();
        for (index, step) in steps.into_iter().enumerate() {
            let number = |target: usize| target as u64 + 1;
            let item = |target: usize| {
                format!("{REPO}/{}/{}", if model.get(target).is_some_and(|item| item.pull) { "pulls" } else { "issues" }, number(target))
            };
            let (reply, expected_status) = match step {
                Step::OpenPull { draft } => {
                    let reply = harness
                        .post(
                            &format!("{REPO}/pulls"),
                            Some(json!({"head": format!("branch-{index}"), "base": "main", "title": "work", "draft": draft})),
                        )
                        .await;
                    model.push(Expected { pull: true, open: true, merged: false, draft, ready: false });
                    (reply, StatusCode::CREATED)
                }
                Step::OpenIssue { ready } => {
                    let labels: Vec<&str> = if ready { vec!["ready"] } else { vec![] };
                    let reply = harness.post(&format!("{REPO}/issues"), Some(json!({"title": "issue", "labels": labels}))).await;
                    model.push(Expected { pull: false, open: true, merged: false, draft: false, ready });
                    (reply, StatusCode::CREATED)
                }
                Step::Merge(target) => {
                    let reply = harness.post(&format!("{REPO}/pulls/{}/merge", number(target)), None).await;
                    let status = match model.get_mut(target) {
                        None => StatusCode::NOT_FOUND,
                        Some(item) if !item.pull => StatusCode::NOT_FOUND,
                        Some(item) if !item.open || item.draft => StatusCode::METHOD_NOT_ALLOWED,
                        Some(item) => {
                            item.open = false;
                            item.merged = true;
                            StatusCode::OK
                        }
                    };
                    (reply, status)
                }
                Step::Close(target) | Step::Reopen(target) => {
                    let open = matches!(step, Step::Reopen(_));
                    let action = if open { "reopen" } else { "close" };
                    let reply = harness.post(&format!("{}/{action}", item(target)), None).await;
                    let status = match model.get_mut(target) {
                        None => StatusCode::NOT_FOUND,
                        Some(item) if open && item.merged => StatusCode::METHOD_NOT_ALLOWED,
                        Some(item) => {
                            item.open = open;
                            StatusCode::OK
                        }
                    };
                    (reply, status)
                }
                Step::Ready(target) => {
                    let reply = harness.post(&format!("{REPO}/pulls/{}/ready", number(target)), None).await;
                    let status = match model.get_mut(target) {
                        Some(item) if item.pull => {
                            item.draft = false;
                            StatusCode::OK
                        }
                        _ => StatusCode::NOT_FOUND,
                    };
                    (reply, status)
                }
                Step::Relabel(target, ready) => {
                    let labels: Vec<&str> = if ready { vec!["ready"] } else { vec!["later"] };
                    let reply = harness
                        .admin(Method::PUT, &format!("{REPO}/issues/{}/labels", number(target)), Some(json!({"labels": labels})))
                        .await;
                    let status = match model.get_mut(target) {
                        Some(item) => {
                            item.ready = ready;
                            StatusCode::OK
                        }
                        None => StatusCode::NOT_FOUND,
                    };
                    (reply, status)
                }
            };
            assert_eq!(reply.status, expected_status, "step {index} {step:?}: {}", reply.body);
            check_api_view(&harness, &model).await;
        }
    });
}

async fn check_api_view(harness: &Harness, model: &[Expected]) {
    let pulls = harness.api_get("/pulls?state=all&limit=50&page=1").await;
    let issues = harness.api_get("/issues?state=all&type=issues&sort=recentupdate&limit=50").await;
    let mut seen: Vec<u64> = numbers(&pulls).into_iter().chain(numbers(&issues)).collect();
    seen.sort_unstable();
    let expected_numbers: Vec<u64> = (1..=model.len() as u64).collect();
    assert_eq!(seen, expected_numbers, "numbers are unique and gapless across pulls and issues");
    for pull in pulls.body.as_array().expect("pulls") {
        let number = pull["number"].as_u64().expect("number");
        let expected = model[number as usize - 1];
        assert!(expected.pull, "#{number} listed as a pull");
        let merged = pull["merged"].as_bool().expect("merged");
        let open = pull["state"] == "open";
        assert!(!merged || !open, "#{number}: merged implies closed");
        assert_eq!((open, merged, pull["draft"].as_bool().expect("draft")), (expected.open, expected.merged, expected.draft), "#{number}");
    }
    for issue in issues.body.as_array().expect("issues") {
        let number = issue["number"].as_u64().expect("number");
        let expected = model[number as usize - 1];
        assert!(!expected.pull, "#{number} listed as an issue");
        assert_eq!(issue["state"] == "open", expected.open, "#{number}");
    }
    let mut ready = numbers(&harness.api_get("/issues?state=open&type=issues&sort=recentupdate&limit=50&labels=ready").await);
    ready.sort_unstable();
    let expected_ready: Vec<u64> =
        model.iter().enumerate().filter(|(_, item)| !item.pull && item.open && item.ready).map(|(index, _)| index as u64 + 1).collect();
    assert_eq!(ready, expected_ready, "the label filter returns exactly the open `ready` issues");
}

// Behaviour: token comparison accepts exactly the expected token. The
// generator draws short tokens over a two-letter alphabet, so equal tokens,
// prefixes, extensions and single-byte differences all occur.
#[hegel::test]
fn tokens_match_only_the_expected_token(tc: hegel::TestCase) {
    let draw = |tc: &hegel::TestCase| -> String {
        (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(4))).map(|_| if tc.draw(gs::booleans()) { 'a' } else { 'b' }).collect()
    };
    let (presented, expected) = (draw(&tc), draw(&tc));
    assert_eq!(crate::api::tokens_match(&presented, &expected), presented == expected);
}
