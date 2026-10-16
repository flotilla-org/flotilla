//! The fake answers every recorded Forgejo request the way the lab Forgejo
//! did (#3374). Each recorded fixture is replayed, request by request, against
//! a fake seeded to the world the recording shows, and the fake's answer is
//! compared with the recorded one on everything flotilla's Forgejo providers
//! read: status, `X-Total-Count`, whether `Link` offers a next page, the item
//! numbers in order, and each item's provider-read fields.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use flotilla_fake_forge::{
    recorded::{RecordedWorld, RECORDED_ORIGIN, RECORDED_TOKEN},
    FakeForge, InProcessClient,
};
use flotilla_provider_runtime::{ChannelLabel, HttpClient};
use serde::Deserialize;
use serde_json::Value;

const PUBLIC_URL: &str = "https://fake-forge.test:3443";
const FORGE: &str = "lab";

#[derive(Deserialize)]
struct Fixture {
    rounds: Vec<Round>,
}

#[derive(Deserialize)]
struct Round {
    interactions: Vec<Interaction>,
}

#[derive(Deserialize)]
struct Interaction {
    channel: String,
    method: String,
    url: String,
    #[serde(default)]
    request_headers: BTreeMap<String, String>,
    status: u16,
    #[serde(default)]
    response_body: Option<String>,
    #[serde(default)]
    response_headers: BTreeMap<String, String>,
}

/// Every recorded Forgejo fixture and the world it shows.
fn fixtures() -> Vec<(PathBuf, RecordedWorld)> {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().expect("crates directory").to_path_buf();
    let change_requests = crates.join("flotilla-change-request/src/fixtures");
    vec![
        (change_requests.join("forgejo_pulls.yaml"), RecordedWorld::LabFlotillaWithoutOpenPulls),
        (change_requests.join("forgejo_ghostty_governor.yaml"), RecordedWorld::GhosttyOpsBeforePulls),
        (change_requests.join("forgejo_head_filter_capability.yaml"), RecordedWorld::GhosttyOpsWithMergedPulls),
        (crates.join("flotilla-issue-tracker/src/fixtures/forgejo_issues.yaml"), RecordedWorld::ZellijIssues),
    ]
}

/// A recorded answer the fake deliberately does not reproduce.
struct Divergence {
    fixture: &'static str,
    url: &'static str,
    /// Item numbers the fake also lists, and why.
    extra_numbers: &'static [u64],
    reason: &'static str,
}

const DIVERGENCES: &[Divergence] = &[Divergence {
    fixture: "forgejo_issues.yaml",
    url: "/repos/fork-issues/zellij/issues?state=all&type=issues&since=2026-01-01T00%3A00%3A00Z&sort=recentupdate&limit=30",
    extra_numbers: &[8],
    reason: "the fixture is synthetic: the same recording lists issue 8 as open and updated at 2026-07-24T11:00:00Z, \
             so an honest forge also lists it as changed since 2026-01-01",
}];

/// The recorded URL as the fake serves it: the lab origin becomes the fake's
/// `lab` mount.
fn fake_url(recorded: &str) -> String {
    let rest = recorded.strip_prefix(RECORDED_ORIGIN).unwrap_or_else(|| panic!("{recorded} is not on the recorded lab origin"));
    format!("{PUBLIC_URL}/{FORGE}{rest}")
}

struct Answer {
    status: u16,
    headers: http::HeaderMap,
    body: Option<Value>,
}

async fn send(fake: &Arc<FakeForge>, method: &str, url: &str, headers: &BTreeMap<String, String>) -> Answer {
    let mut request = reqwest::Request::new(method.parse().expect("method"), url.parse().expect("URL"));
    for (name, value) in headers {
        request
            .headers_mut()
            .insert(reqwest::header::HeaderName::from_bytes(name.as_bytes()).expect("header name"), value.parse().expect("header value"));
    }
    let response = InProcessClient::new(Arc::clone(fake)).execute(request, &ChannelLabel::Default).await.expect("fake is reachable");
    let body = (!response.body().is_empty()).then(|| serde_json::from_slice(response.body()).expect("fake answers JSON"));
    Answer { status: response.status().as_u16(), headers: response.headers().clone(), body }
}

fn has_next(link: Option<&str>) -> bool {
    link.is_some_and(|link| link.contains("rel=\"next\""))
}

fn items(body: Option<&Value>) -> Vec<Value> {
    match body {
        None => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(item) => vec![item.clone()],
    }
}

/// Fields flotilla's Forgejo providers read from a pull request or issue.
const PROVIDER_FIELDS: &[&str] = &[
    "/number",
    "/title",
    "/body",
    "/state",
    "/draft",
    "/merged",
    "/merged_at",
    "/created_at",
    "/updated_at",
    "/closed_at",
    "/head/ref",
    "/head/label",
    "/base/ref",
    "/user/login",
    "/pull_request",
];

fn names(item: &Value, list: &str, field: &str) -> Option<Vec<String>> {
    let entries = item.pointer(list)?;
    Some(
        entries
            .as_array()
            .map(|entries| entries.iter().filter_map(|entry| entry[field].as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
    )
}

fn assert_same_item(context: &str, recorded: &Value, fake: &Value) {
    for field in PROVIDER_FIELDS {
        if let Some(expected) = recorded.pointer(field) {
            assert_eq!(fake.pointer(field), Some(expected), "{context}: field {field}");
        }
    }
    for (list, field) in [("/labels", "name"), ("/assignees", "login"), ("/requested_reviewers", "login")] {
        if let Some(expected) = names(recorded, list, field) {
            assert_eq!(names(fake, list, field).unwrap_or_default(), expected, "{context}: {list} {field}s");
        }
    }
}

fn number(item: &Value) -> u64 {
    item["number"].as_u64().expect("item number")
}

// Behaviour (#3374): every request the Forgejo providers made against the lab
// Forgejo, replayed with its recorded headers against a fake seeded to the
// recorded world, gets the recorded status, total count, next-page signal,
// item numbers in order and provider-read fields. The only exception is a
// listed divergence, where the fake must differ exactly as described.
#[tokio::test]
async fn fake_reproduces_every_recorded_forgejo_answer() {
    let mut replayed = 0;
    let mut applied = vec![0_usize; DIVERGENCES.len()];
    for (path, world) in fixtures() {
        let file = path.file_name().expect("fixture name").to_string_lossy().into_owned();
        let fixture: Fixture = serde_yml::from_str(&std::fs::read_to_string(&path).expect("read fixture")).expect("parse fixture");
        let fake = world.seed(PUBLIC_URL, FORGE, RECORDED_TOKEN);
        for (index, interaction) in fixture.rounds.iter().flat_map(|round| &round.interactions).enumerate() {
            assert_eq!(interaction.channel, "http", "{file}: Forgejo fixtures record HTTP only");
            let context = format!("{file} #{index} {} {}", interaction.method, interaction.url);
            let answer = send(&fake, &interaction.method, &fake_url(&interaction.url), &interaction.request_headers).await;
            assert_eq!(answer.status, interaction.status, "{context}: status");
            let matched = DIVERGENCES.iter().position(|divergence| divergence.fixture == file && interaction.url.ends_with(divergence.url));
            if let Some(index) = matched {
                applied[index] += 1;
            }
            let divergence = matched.map(|index| &DIVERGENCES[index]);
            let extra = divergence.map_or(&[][..], |divergence| divergence.extra_numbers);

            let recorded_total = interaction.response_headers.get("x-total-count").map(|total| total.parse::<usize>().expect("count"));
            let fake_total =
                answer.headers.get("x-total-count").map(|total| total.to_str().expect("ASCII").parse::<usize>().expect("count"));
            assert_eq!(fake_total, recorded_total.map(|total| total + extra.len()), "{context}: x-total-count");
            assert_eq!(
                has_next(answer.headers.get("link").and_then(|link| link.to_str().ok())),
                has_next(interaction.response_headers.get("link").map(String::as_str)),
                "{context}: next page"
            );

            let recorded_body: Option<Value> = interaction
                .response_body
                .as_deref()
                .filter(|body| !body.trim().is_empty())
                .map(|body| serde_json::from_str(body).expect("recorded JSON"));
            let recorded_items = items(recorded_body.as_ref());
            let fake_items = items(answer.body.as_ref());
            let fake_numbers: Vec<u64> = fake_items.iter().map(number).collect();
            assert_eq!(
                fake_numbers.iter().copied().filter(|number| !extra.contains(number)).collect::<Vec<_>>(),
                recorded_items.iter().map(number).collect::<Vec<_>>(),
                "{context}: item numbers"
            );
            for missing in extra {
                assert!(
                    fake_numbers.contains(missing),
                    "{context}: divergence `{}` no longer holds",
                    divergence.expect("divergence").reason
                );
            }
            for recorded in &recorded_items {
                let fake_item = fake_items.iter().find(|item| number(item) == number(recorded)).expect("matched above");
                assert_same_item(&format!("{context} item {}", number(recorded)), recorded, fake_item);
            }
            replayed += 1;
        }
        let (refusals, refused) = fake.refusals();
        assert_eq!(refused, 0, "{file}: recorded requests are within the contract, but the fake refused {refusals:?}");
    }
    assert_eq!(replayed, 12, "every recorded interaction is replayed");
    // A divergence that no longer matches a recording would hide nothing but
    // could hide a future mismatch: each must apply to exactly one interaction.
    for (divergence, count) in DIVERGENCES.iter().zip(applied) {
        assert_eq!(count, 1, "divergence for {} {} applies to {count} recorded interactions", divergence.fixture, divergence.url);
    }
}

// Behaviour (#3374): the fixtures cover every Forgejo recording flotilla's
// providers keep, so a new recording cannot skip the conformance check.
#[test]
fn every_forgejo_fixture_has_a_recorded_world() {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().expect("crates directory").to_path_buf();
    let covered: Vec<PathBuf> = fixtures().into_iter().map(|(path, _)| path).collect();
    for dir in ["flotilla-change-request/src/fixtures", "flotilla-issue-tracker/src/fixtures"] {
        for entry in std::fs::read_dir(crates.join(dir)).expect("fixture directory") {
            let path = entry.expect("fixture entry").path();
            if path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("forgejo_")) {
                assert!(covered.contains(&path), "{} has no RecordedWorld in fixture_conformance", path.display());
            }
        }
    }
    assert_eq!(covered.len(), RecordedWorld::ALL.len());
}

fn recorded_headers() -> BTreeMap<String, String> {
    BTreeMap::from([("accept".into(), "application/json".into()), ("authorization".into(), format!("token {RECORDED_TOKEN}"))])
}

// Behaviour (#3374): requests outside the recorded shapes are refused and
// recorded, never answered loosely. Each case changes one thing about a
// recorded request: an extra or repeated parameter, a parameter value no
// provider sends, a route no provider uses, a missing or wrong credential, or
// a missing `Accept`.
#[tokio::test]
async fn requests_outside_the_recorded_shapes_are_refused() {
    let fake = RecordedWorld::GhosttyOpsWithMergedPulls.seed(PUBLIC_URL, FORGE, RECORDED_TOKEN);
    let pulls = format!("{RECORDED_ORIGIN}/api/v1/repos/robert/ghostty-ops/pulls");
    let issues = format!("{RECORDED_ORIGIN}/api/v1/repos/fork-issues/zellij/issues");
    let without = |name: &str| {
        let mut headers = recorded_headers();
        headers.remove(name);
        headers
    };
    let wrong_token = BTreeMap::from([("accept".into(), "application/json".into()), ("authorization".into(), "token other".into())]);
    let cases: Vec<(String, BTreeMap<String, String>, u16)> = vec![
        (format!("{pulls}?state=all&limit=50&page=1&sort=newest"), recorded_headers(), 400),
        (format!("{pulls}?state=all&limit=50&page=1&page=2"), recorded_headers(), 400),
        (format!("{pulls}?state=merged&limit=50&page=1"), recorded_headers(), 400),
        (format!("{pulls}?state=all&limit=51&page=1"), recorded_headers(), 400),
        (format!("{pulls}?state=all&limit=50"), recorded_headers(), 400),
        (format!("{issues}?state=open&type=issues&sort=recentupdate&page=1&limit=30&milestone=v1"), recorded_headers(), 400),
        (format!("{issues}?state=open&type=pulls&sort=recentupdate&page=1&limit=30"), recorded_headers(), 400),
        (format!("{RECORDED_ORIGIN}/api/v1/repos/robert/ghostty-ops/pulls/2/files"), recorded_headers(), 404),
        (format!("{pulls}?state=all&limit=50&page=1"), without("authorization"), 401),
        (format!("{pulls}?state=all&limit=50&page=1"), wrong_token, 401),
        (format!("{pulls}?state=all&limit=50&page=1"), without("accept"), 406),
    ];
    for (index, (url, headers, status)) in cases.iter().enumerate() {
        let answer = send(&fake, "GET", &fake_url(url), headers).await;
        assert_eq!(answer.status, *status, "{url} with {headers:?}");
        assert!(answer.body.as_ref().is_some_and(|body| body["message"].is_string()), "{url}: refusal explains itself");
        assert_eq!(fake.refusals().1, index as u64 + 1, "{url}: refusal is recorded");
    }
}
