use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_forge_forgejo::{ForgejoClient, ForgejoIssueProviderConfig};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    Issue, IssueChangeset, IssueRef, IssueSource, IssueState,
};

use flotilla_provider_runtime::run;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::HttpClient;

const MAX_FORGEJO_LIMIT: usize = 50;

pub struct ForgejoIssueProvider {
    runner: Arc<dyn CommandRunner>,
    forge: ForgejoClient,
}

impl ForgejoIssueProvider {
    pub fn new(http: Arc<dyn HttpClient>, runner: Arc<dyn CommandRunner>, config: ForgejoIssueProviderConfig) -> Self {
        Self { forge: ForgejoClient::new(http, config), runner }
    }

    fn html_url(&self, reference: &IssueRef) -> String {
        format!("{}/{}/issues/{}", self.forge.config.service_url.trim_end_matches('/'), reference.source.scope, reference.id)
    }
}

fn normalized_service(value: &str) -> &str {
    value.trim_end_matches('/')
}

pub fn is_forgejo_source(source: &IssueSource, service_url: &str) -> bool {
    let service = normalized_service(&source.service);
    service == normalized_service(service_url)
}

fn clamp_limit(count: usize) -> usize {
    if count > MAX_FORGEJO_LIMIT {
        tracing::warn!(requested = %count, max = MAX_FORGEJO_LIMIT, "Forgejo API page size capped");
        MAX_FORGEJO_LIMIT
    } else {
        count
    }
}

fn parse_issue(source: &IssueSource, value: &serde_json::Value, fetched_at: DateTime<Utc>) -> Option<Issue> {
    if !value["pull_request"].is_null() {
        return None;
    }
    let number = value["number"].as_i64()?;
    let title = value["title"].as_str()?.to_string();
    let body = value["body"].as_str().map(str::to_string);
    let state = match value["state"].as_str()? {
        "open" => IssueState::Open,
        "closed" => IssueState::Closed,
        _ => return None,
    };
    let labels = value["labels"]
        .as_array()
        .map(|labels| labels.iter().filter_map(|label| label["name"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let assignees = value["assignees"]
        .as_array()
        .map(|items| items.iter().filter_map(|item| item["login"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let as_of = value["updated_at"].as_str().and_then(|value| value.parse::<DateTime<Utc>>().ok()).unwrap_or(fetched_at);
    let id = number.to_string();
    Some(
        Issue::builder()
            .reference(IssueRef { source: source.clone(), id: id.clone() })
            .title(title)
            .maybe_body(body)
            .state(state)
            .labels(labels)
            .assignees(assignees)
            .as_of(as_of)
            .observed_at(fetched_at)
            .provider_name("forgejo".into())
            .provider_display_name("Forgejo".into())
            .build(),
    )
}

#[cfg(target_os = "macos")]
fn browser_open_command(url: &str) -> (&'static str, Vec<String>) {
    ("open", vec![url.to_string()])
}

#[cfg(target_os = "windows")]
fn browser_open_command(url: &str) -> (&'static str, Vec<String>) {
    ("cmd", vec!["/C".into(), "start".into(), String::new(), url.to_string()])
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn browser_open_command(url: &str) -> (&'static str, Vec<String>) {
    ("xdg-open", vec![url.to_string()])
}

#[async_trait]
impl super::IssueProvider for ForgejoIssueProvider {
    fn supports(&self, source: &IssueSource) -> bool {
        is_forgejo_source(source, &self.forge.config.service_url)
    }

    async fn query(&self, source: &IssueSource, params: &IssueQuery, page: u32, count: usize) -> Result<IssueResultPage, String> {
        let limit = clamp_limit(count);
        let mut query = vec![
            ("state", "open".to_string()),
            ("type", "issues".to_string()),
            ("sort", "recentupdate".to_string()),
            ("page", page.to_string()),
            ("limit", limit.to_string()),
        ];
        if let Some(label) = &params.label {
            query.push(("labels", label.clone()));
        }
        if let Some(search) = &params.search {
            query.push(("q", search.clone()));
        }
        for (field, values) in &params.match_fields {
            query.push((field, values.join(",")));
        }
        let (value, has_more) = self.forge.get_json(&format!("repos/{}/issues", source.scope), &query).await?;
        let fetched_at = Utc::now();
        let raw_items = value.as_array().ok_or("Forgejo issue list response was not an array")?;
        let items = raw_items.iter().filter_map(|value| parse_issue(source, value, fetched_at)).collect();
        Ok(IssueResultPage { items, total: None, has_more })
    }

    async fn fetch_by_id(&self, reference: &IssueRef) -> Result<Issue, String> {
        let (value, _) = self.forge.get_json(&format!("repos/{}/issues/{}", reference.source.scope, reference.id), &[]).await?;
        parse_issue(&reference.source, &value, Utc::now()).ok_or_else(|| format!("failed to parse Forgejo issue {}", reference.id))
    }

    async fn list_changed_since(&self, source: &IssueSource, since: &str, count: usize) -> Result<IssueChangeset, String> {
        let limit = clamp_limit(count);
        let query = vec![
            ("state", "all".to_string()),
            ("type", "issues".to_string()),
            ("since", since.to_string()),
            ("sort", "recentupdate".to_string()),
            ("limit", limit.to_string()),
        ];
        let (value, has_more) = self.forge.get_json(&format!("repos/{}/issues", source.scope), &query).await?;
        let fetched_at = Utc::now();
        let raw_items = value.as_array().ok_or("Forgejo changed-since response was not an array")?;
        let mut updated = Vec::new();
        let mut closed = Vec::new();
        for value in raw_items {
            match value["state"].as_str() {
                Some("open") => {
                    if let Some(issue) = parse_issue(source, value, fetched_at) {
                        updated.push(issue);
                    }
                }
                Some("closed") => {
                    if let Some(number) = value["number"].as_i64() {
                        closed.push(IssueRef { source: source.clone(), id: number.to_string() });
                    }
                }
                _ => {}
            }
        }
        Ok(IssueChangeset { updated, closed, has_more })
    }

    async fn open_in_browser(&self, reference: &IssueRef) -> Result<(), String> {
        let url = self.html_url(reference);
        let (cmd, args) = browser_open_command(&url);
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        run!(self.runner, cmd, &args, Path::new("/"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use flotilla_fake_forge::{
        recorded::{ManualClock, RecordedWorld},
        Clock, FakeForge, ForgeSettings, InProcessClient, ItemEdit, ItemState, NewIssue,
    };
    use flotilla_forge_forgejo::ForgejoAuth;
    use std::path::PathBuf;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use super::*;
    use flotilla_provider_runtime::ChannelLabel;
    use flotilla_replay_testkit as replay;
    use flotilla_replay_testkit::testing::MockRunner;
    use flotilla_replay_testkit::Masks;

    use crate::testing::assert_provider_contract;
    use crate::testing::fixture_path;
    use crate::IssueProvider;

    #[test]
    fn parses_issue_title_and_assignees_from_existing_rest_response() {
        let source = IssueSource { service: "https://forgejo.example".into(), scope: "team/repo".into() };
        let observed_at = "2026-10-01T12:00:00Z".parse().expect("time");
        let raw = serde_json::json!({"number": 7, "title": "Fix refresh", "state": "open", "assignees": [{"login": "alice"}]});
        let issue = parse_issue(&source, &raw, observed_at).expect("parse issue");
        assert_eq!(issue.title, "Fix refresh");
        assert_eq!(issue.assignees, ["alice"]);
        assert_eq!(issue.observed_at, Some(observed_at));
    }
    type RecordedRequest = (String, String, HashMap<String, String>);

    struct MockHttp {
        responses: Mutex<Vec<http::Response<bytes::Bytes>>>,
        requests: Mutex<Vec<RecordedRequest>>,
    }

    impl MockHttp {
        fn new(responses: Vec<http::Response<bytes::Bytes>>) -> Self {
            Self { responses: Mutex::new(responses), requests: Mutex::new(Vec::new()) }
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().expect("request lock poisoned").clone()
        }
    }

    #[async_trait]
    impl HttpClient for MockHttp {
        async fn execute(&self, request: reqwest::Request, _label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
            let headers =
                request.headers().iter().map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_string())).collect();
            self.requests.lock().expect("request lock poisoned").push((request.method().to_string(), request.url().to_string(), headers));
            Ok(self.responses.lock().expect("response lock poisoned").remove(0))
        }
    }

    const LAB_SERVICE_URL: &str = "https://forgejo.lab.flotilla.work";

    fn source() -> IssueSource {
        IssueSource { service: LAB_SERVICE_URL.into(), scope: "fork-issues/zellij".into() }
    }

    fn contract_source() -> IssueSource {
        source()
    }

    fn auth() -> ForgejoAuth {
        ForgejoAuth { token: "test-token".into(), token_path: PathBuf::from("/tmp/lab-forgejo-test-token") }
    }

    fn response(body: &str, has_next: bool) -> http::Response<bytes::Bytes> {
        let mut builder = http::Response::builder().status(200);
        if has_next {
            builder =
                builder.header("link", "<https://forgejo.lab.flotilla.work/api/v1/repos/fork-issues/zellij/issues?page=2>; rel=\"next\"");
        }
        builder.body(bytes::Bytes::from(body.to_string())).expect("response")
    }

    fn provider(http: Arc<dyn HttpClient>) -> ForgejoIssueProvider {
        ForgejoIssueProvider::new(
            http,
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(LAB_SERVICE_URL.into(), None, auth()),
        )
    }

    fn fixture(name: &str) -> String {
        fixture_path(name)
    }

    fn replay_auth() -> ForgejoAuth {
        if !replay::is_live() {
            return ForgejoAuth { token: "fixture-token".into(), token_path: PathBuf::from("fixture-token") };
        }
        let token_path = std::env::var_os("LAB_FORGEJO_TOKEN_PATH")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".config/lab-forgejo-coder-token")))
            .expect("set LAB_FORGEJO_TOKEN_PATH for live Forgejo recording");
        let token = std::fs::read_to_string(&token_path)
            .unwrap_or_else(|error| panic!("read Forgejo token {}: {error}", token_path.display()))
            .trim()
            .to_string();
        ForgejoAuth { token, token_path }
    }

    #[tokio::test]
    async fn record_replay_satisfies_provider_contract() {
        let auth = replay_auth();
        let mut masks = Masks::new();
        masks.add(&auth.token, "<LAB_FORGEJO_TOKEN>");
        let session = replay::test_session(&fixture("forgejo_issues.yaml"), masks);
        let http = replay::test_http_client(&session);
        let provider = ForgejoIssueProvider::new(
            http,
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(LAB_SERVICE_URL.into(), None, auth),
        );

        assert_provider_contract(&provider, &contract_source(), "9", "2026-01-01T00:00:00Z").await;

        session.finish();
    }

    const FAKE_URL: &str = "https://fake-forge.test:3443";
    const FAKE_TOKEN: &str = "fake-forge-token";

    /// The real provider, configured as a rehearsal Forge record points it at
    /// the fake's `lab` mount, reaching it through the in-process HTTP seam.
    fn fake_provider(fake: &Arc<FakeForge>) -> (ForgejoIssueProvider, IssueSource) {
        let provider = ForgejoIssueProvider::new(
            Arc::new(InProcessClient::new(Arc::clone(fake))),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(
                fake.service_url("lab"),
                None,
                ForgejoAuth { token: FAKE_TOKEN.into(), token_path: PathBuf::from("fake-forge-token") },
            ),
        );
        let source = IssueSource { service: fake.service_url("lab"), scope: RecordedWorld::ZellijIssues.repo_slug().into() };
        assert!(provider.supports(&source), "a fake-backed Forge record's source belongs to the provider");
        (provider, source)
    }

    fn assert_no_refusals(fake: &FakeForge) {
        let (refusals, count) = fake.refusals();
        assert_eq!(count, 0, "the fake refused provider requests: {refusals:?}");
    }

    fn ids(issues: &[Issue]) -> Vec<&str> {
        issues.iter().map(|issue| issue.reference.id.as_str()).collect()
    }

    // Behaviour (#3374): the shared issue-provider contract that runs on the
    // recording above also holds against the fake seeded to the recorded world.
    #[tokio::test]
    async fn fake_forge_satisfies_provider_contract() {
        let fake = RecordedWorld::ZellijIssues.seed(FAKE_URL, "lab", FAKE_TOKEN);
        let (provider, source) = fake_provider(&fake);
        assert_provider_contract(&provider, &source, "9", "2026-01-01T00:00:00Z").await;
        assert_no_refusals(&fake);
    }

    // Behaviour (#3374): against the recorded world, the provider reads the
    // same issues the recording shows (open 9 then 8, newest update first,
    // with their titles, labels and update times), fetches one by number,
    // partitions changes since a time into open updates and closed numbers,
    // and narrows queries by label and by text search.
    #[tokio::test]
    async fn fake_forge_answers_issue_queries_over_the_recorded_world() {
        let fake = RecordedWorld::ZellijIssues.seed(FAKE_URL, "lab", FAKE_TOKEN);
        let (provider, source) = fake_provider(&fake);
        let page = provider.query(&source, &IssueQuery::default(), 1, 30).await.expect("query");
        assert_eq!(ids(&page.items), ["9", "8"]);
        assert!(!page.has_more);
        let titles: Vec<_> =
            page.items.iter().map(|issue| (issue.title.as_str(), issue.labels.clone(), issue.as_of.to_rfc3339())).collect();
        assert_eq!(
            titles,
            [
                ("Wire Forgejo issue provider into convoy startup", vec!["ready".to_string()], "2026-07-24T12:00:00+00:00".to_string()),
                ("List Forgejo issues by label", vec!["provider".to_string()], "2026-07-24T11:00:00+00:00".to_string()),
            ]
        );
        let fetched = provider.fetch_by_id(&IssueRef { source: source.clone(), id: "7".into() }).await.expect("fetch closed issue");
        assert_eq!((fetched.title.as_str(), fetched.state), ("Closed synthetic Forgejo issue", IssueState::Closed));
        assert!(provider.fetch_by_id(&IssueRef { source: source.clone(), id: "99".into() }).await.expect_err("missing").contains("404"));

        let changes = provider.list_changed_since(&source, "2026-01-01T00:00:00Z", 30).await.expect("changes");
        assert_eq!(ids(&changes.updated), ["9", "8"]);
        assert_eq!(changes.closed, [IssueRef { source: source.clone(), id: "7".into() }]);
        let later = provider.list_changed_since(&source, "2026-07-24T10:30:00Z", 30).await.expect("later changes");
        assert_eq!((ids(&later.updated), later.closed.len()), (vec!["9", "8"], 0));

        for (label, search, expected) in [
            (Some("ready"), None, vec!["9"]),
            (Some("provider"), None, vec!["8"]),
            (Some("absent"), None, vec![]),
            (None, Some("by label"), vec!["8"]),
            (None, Some("CONVOY"), vec!["9"]),
            (Some("ready"), Some("by label"), vec![]),
        ] {
            let query =
                IssueQuery { label: label.map(str::to_string), search: search.map(str::to_string), match_fields: Default::default() };
            let page = provider.query(&source, &query, 1, 30).await.expect("filtered query");
            assert_eq!(ids(&page.items), expected, "label {label:?}, search {search:?}");
        }
        assert_no_refusals(&fake);
    }

    // Finding (#3374): `IssueQuery.match_fields` sends each project-declared
    // field name as a query parameter. Forgejo silently ignores parameters it
    // does not know, so the provider's match fields filter nothing there; the
    // fake refuses the unrecorded shape instead, which makes the latent
    // behaviour visible. This pins the refusal; the provider is unchanged.
    #[tokio::test]
    async fn fake_forge_refuses_match_field_parameters() {
        let fake = RecordedWorld::ZellijIssues.seed(FAKE_URL, "lab", FAKE_TOKEN);
        let (provider, source) = fake_provider(&fake);
        let query = IssueQuery { match_fields: [("priority".to_string(), vec!["high".to_string()])].into(), ..IssueQuery::default() };
        let error = provider.query(&source, &query, 1, 30).await.expect_err("unrecorded parameter");
        assert!(error.contains("400"), "{error}");
        let (refusals, count) = fake.refusals();
        assert_eq!(count, 1);
        assert!(refusals[0].reason.contains("`priority`"), "{refusals:?}");
    }

    const LABELS: [&str; 3] = ["ready", "provider", "bug"];

    // Behaviour (#3374): paging through a filtered query with the provider
    // against the fake yields exactly the open issues carrying the label,
    // most recently updated first, each page full until the last, with
    // `has_more` (from the `Link` next relation) false only on the last page.
    // The generator draws 0..=12 issues, each with any subset of three labels,
    // open or closed, some edited later so update order differs from number
    // order; a label filter or none; and a page size of 1..=5, so listings fit
    // one page, fill pages exactly, and spill over.
    #[hegel::test]
    fn fake_forge_paged_label_queries_agree_with_the_forge(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(12));
        let issues: Vec<(usize, bool, bool)> = (0..count)
            .map(|_| (tc.draw(gs::integers::<usize>().min_value(0).max_value(7)), tc.draw(gs::booleans()), tc.draw(gs::booleans())))
            .collect();
        let label = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let label = LABELS.get(label).copied();
        let page_size = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let clock = Arc::new(ManualClock::new("2026-07-24T00:00:00Z".parse().expect("time")));
            let fake = Arc::new(FakeForge::new(FAKE_URL, clock.clone()));
            fake.add_forge("lab", ForgeSettings { token: FAKE_TOKEN.into(), login: "flotilla-bot".into() }).expect("forge");
            let repo = RecordedWorld::ZellijIssues.repository("lab");
            // Whole minutes apart, so second-precision update times are exact.
            let tick = || clock.set(clock.now() + chrono::Duration::minutes(1));
            let mut numbers = Vec::new();
            for (index, (mask, _, _)) in issues.iter().enumerate() {
                tick();
                let labels =
                    LABELS.iter().enumerate().filter(|(bit, _)| mask & (1 << bit) != 0).map(|(_, name)| name.to_string()).collect();
                let new = NewIssue { title: format!("Issue {index}"), labels, ..NewIssue::default() };
                numbers.push(fake.create_issue(&repo, new).expect("issue").number);
            }
            for (number, (_, closed, edited)) in numbers.iter().zip(&issues) {
                if *edited || *closed {
                    tick();
                    let state = closed.then_some(ItemState::Closed);
                    fake.edit(&repo, *number, ItemEdit { body: Some("edited".into()), state, ..ItemEdit::default() }).expect("edit");
                }
            }
            let mut expected: Vec<_> = numbers
                .iter()
                .map(|number| fake.item(&repo, *number).expect("issue"))
                .filter(|item| item.state == ItemState::Open && label.is_none_or(|label| item.labels.iter().any(|name| name == label)))
                .collect();
            expected.sort_by_key(|item| std::cmp::Reverse(item.updated_at));

            let (provider, source) = fake_provider(&fake);
            let query = IssueQuery { label: label.map(str::to_string), ..IssueQuery::default() };
            let mut seen: Vec<(String, Vec<String>, DateTime<Utc>)> = Vec::new();
            for page_number in 1..=20 {
                let page = provider.query(&source, &query, page_number, page_size).await.expect("page");
                seen.extend(page.items.iter().map(|issue| (issue.reference.id.clone(), issue.labels.clone(), issue.as_of)));
                if !page.has_more {
                    break;
                }
                assert_eq!(page.items.len(), page_size, "a page with more after it is full");
            }
            let expected: Vec<_> = expected.into_iter().map(|item| (item.number.to_string(), item.labels, item.updated_at)).collect();
            assert_eq!(seen, expected);
            assert_no_refusals(&fake);
        });
    }

    #[tokio::test]
    async fn query_sends_forgejo_issue_filters() {
        let http = Arc::new(MockHttp::new(vec![response("[]", false)]));
        let provider = provider(http.clone());

        provider
            .query(
                &source(),
                &IssueQuery { search: Some("anchor".into()), label: Some("ready".into()), match_fields: Default::default() },
                2,
                75,
            )
            .await
            .expect("query should succeed");

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "GET");
        assert_eq!(
            requests[0].1,
            "https://forgejo.lab.flotilla.work/api/v1/repos/fork-issues/zellij/issues?state=open&type=issues&sort=recentupdate&page=2&limit=50&labels=ready&q=anchor"
        );
        assert_eq!(requests[0].2.get("authorization").map(String::as_str), Some("token test-token"));
    }

    #[tokio::test]
    async fn fetch_by_id_preserves_source_and_body() {
        let http = Arc::new(MockHttp::new(vec![response(
            r#"{"number":9,"title":"Atomic tabs","body":"Details","state":"open","labels":[{"name":"ready"}],"updated_at":"2026-07-24T10:00:00Z"}"#,
            false,
        )]));
        let provider = provider(http);

        let reference = IssueRef { source: source(), id: "9".into() };
        let issue = provider.fetch_by_id(&reference).await.expect("fetch should succeed");

        assert_eq!(issue.reference, reference);
        assert_eq!(issue.title, "Atomic tabs");
        assert_eq!(issue.body.as_deref(), Some("Details"));
        assert_eq!(issue.labels, vec!["ready"]);
        assert_eq!(issue.provider_name, "forgejo");
    }

    #[tokio::test]
    async fn changed_since_partitions_open_and_closed() {
        let http = Arc::new(MockHttp::new(vec![response(
            r#"[
                {"number":9,"title":"Open","state":"open","labels":[]},
                {"number":10,"title":"Closed","state":"closed","labels":[]}
            ]"#,
            true,
        )]));
        let provider = provider(http);

        let changes = provider.list_changed_since(&source(), "2026-07-01T00:00:00Z", 10).await.expect("changes should succeed");

        assert_eq!(changes.updated.iter().map(|issue| issue.reference.id.as_str()).collect::<Vec<_>>(), vec!["9"]);
        assert_eq!(changes.closed, vec![IssueRef { source: source(), id: "10".into() }]);
        assert!(changes.has_more);
    }

    #[test]
    fn supports_configured_forgejo_service_and_generic_alias() {
        let provider = provider(Arc::new(MockHttp::new(vec![])));

        assert!(provider.supports(&IssueSource { service: "https://forgejo.lab.flotilla.work".into(), scope: "fork-issues/zellij".into() }));
        assert!(provider.supports(&IssueSource { service: LAB_SERVICE_URL.into(), scope: "fork-issues/zellij".into() }));
        assert!(
            !provider.supports(&IssueSource { service: "https://other-forgejo.example.test".into(), scope: "fork-issues/zellij".into() })
        );
        assert!(!provider.supports(&IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() }));
    }

    #[tokio::test]
    async fn open_in_browser_uses_platform_opener_with_source_scope() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let provider = ForgejoIssueProvider::new(
            Arc::new(MockHttp::new(vec![])),
            runner.clone(),
            ForgejoIssueProviderConfig::new(LAB_SERVICE_URL.into(), None, auth()),
        );
        let reference = IssueRef { source: source(), id: "9".into() };

        provider.open_in_browser(&reference).await.expect("open in browser should succeed");

        let url = "https://forgejo.lab.flotilla.work/fork-issues/zellij/issues/9";
        let (expected_cmd, expected_args) = browser_open_command(url);
        assert_eq!(runner.calls(), vec![(expected_cmd.into(), expected_args)]);
    }
}
