use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    Issue, IssueChangeset, IssueRef, IssueSource, IssueState,
};

use crate::providers::{
    gh_api_get, gh_api_get_with_headers,
    github_api::{clamp_per_page, GhApi},
    github_poll::PollCache,
    run, CommandRunner,
};

const INCREMENTAL_PAGE_SIZE: usize = 100;

pub struct GitHubIssueProvider {
    api: Arc<dyn GhApi>,
    runner: Arc<dyn CommandRunner>,
    host_root: Box<Path>,
    poll: PollCache,
}

impl GitHubIssueProvider {
    pub fn with_poll_directory(mut self, directory: std::path::PathBuf) -> Self {
        self.poll = self.poll.with_directory(directory);
        self
    }

    async fn issue_detail(&self, reference: &IssueRef, fields: &str) -> Result<(serde_json::Value, serde_json::Value), String> {
        let key = format!("{}/issue/{}/{}", reference.source.scope, reference.id, fields);
        let endpoint = format!("repos/{}/issues/{}", reference.source.scope, reference.id);
        let response = gh_api_get_with_headers!(self.api, &endpoint, &self.host_root)?;
        let item: serde_json::Value = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
        let rest_revision = serde_json::to_string(&(response.etag, &item)).map_err(|e| e.to_string())?;
        let mut cache = self.poll.state.lock().await;
        let state = cache.entry(key.clone()).or_insert(self.poll.load(&key)?);
        let revision = item["updated_at"].as_str().ok_or("issue lacks updated_at")?;
        if state.issues.revisions.get(&reference.id).is_some_and(|prior| prior == revision)
            && state.issues.check_revisions.get(&reference.id) == Some(&rest_revision)
        {
            if let Some(detail) = state.issues.details.get(&reference.id) {
                return Ok((detail.clone(), item));
            }
        }
        let raw = run!(
            self.runner,
            "gh",
            &["issue", "view", &reference.id, "--repo", &reference.source.scope, "--json", fields],
            &self.host_root
        )?;
        let detail = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let mut next = state.clone();
        next.issues.commit(&item, detail)?;
        next.issues.check_revisions.insert(reference.id.clone(), rest_revision);
        self.poll.save(&key, &next)?;
        *state = next;
        state.issues.details.get(&reference.id).cloned().map(|detail| (detail, item)).ok_or("issue detail was not cached".into())
    }

    async fn poll_board(&self, source: &IssueSource) -> Result<flotilla_protocol::DispatchBoardRepository, String> {
        let mut cache = self.poll.state.lock().await;
        let existing = cache.entry(source.scope.clone()).or_insert(self.poll.load(&source.scope)?);
        let mut next = existing.clone();
        let issues = next.issues.changes(self.api.as_ref(), &self.host_root, &source.scope, "issues").await?;
        let pulls = next.pulls.changes(self.api.as_ref(), &self.host_root, &source.scope, "pulls").await?;
        let mut pull_jobs = pulls.into_iter().map(|item| (item["number"].to_string(), item)).collect::<std::collections::BTreeMap<_, _>>();
        let mut pull_items = next.pulls.items.clone();
        pull_items.extend(pull_jobs.clone());
        for (id, item) in pull_items {
            if item["state"] != "open" {
                continue;
            }
            let Some(sha) = item["head"]["sha"].as_str() else {
                continue;
            };
            let mut checks = Vec::new();
            for suffix in [format!("commits/{sha}/status"), format!("commits/{sha}/check-runs?per_page=100")] {
                let endpoint = format!("repos/{}/{suffix}", source.scope);
                let response = gh_api_get_with_headers!(self.api, &endpoint, &self.host_root)?;
                checks.push((response.etag, response.body));
            }
            let revision = serde_json::to_string(&checks).map_err(|e| e.to_string())?;
            if next.pulls.check_revisions.get(&id) != Some(&revision) {
                pull_jobs.insert(id.clone(), item);
                next.pulls.check_revisions.insert(id, revision);
            }
        }
        for (kind, items) in [("issue", issues), ("pr", pull_jobs.into_values().collect())] {
            for item in items {
                let id = item["number"].as_u64().ok_or("poll item lacks number")?.to_string();
                let fields = if kind == "issue" {
                    "number,title,state,url,updatedAt,closedAt,labels,blockedBy,closedByPullRequestsReferences,parent,issueType"
                } else {
                    "number,state,url,mergedAt,mergeStateStatus,statusCheckRollup"
                };
                // GraphQL is limited to fields REST cannot provide, for this revision.
                let raw = run!(self.runner, "gh", &[kind, "view", &id, "--repo", &source.scope, "--json", fields], &self.host_root)?;
                let detail = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
                let collection = if kind == "issue" { &mut next.issues } else { &mut next.pulls };
                collection.commit(&item, detail)?;
            }
        }
        // Validate native completeness before committing cursor advancement.
        let board = parse_board(source, next.issues.details.values().cloned().collect(), next.pulls.details.values().cloned().collect())?;
        self.poll.save(&source.scope, &next)?;
        *existing = next;
        Ok(board)
    }

    pub fn new(api: Arc<dyn GhApi>, runner: Arc<dyn CommandRunner>, host_root: impl Into<Box<Path>>) -> Self {
        Self { api, runner, host_root: host_root.into(), poll: Default::default() }
    }
}

fn parse_board(
    source: &IssueSource,
    issues: Vec<serde_json::Value>,
    prs: Vec<serde_json::Value>,
) -> Result<flotilla_protocol::DispatchBoardRepository, String> {
    use flotilla_protocol::{DispatchBoardDependency, DispatchBoardIssue, DispatchBoardPullRequest, DispatchBoardRepository};
    let issues = issues
        .iter()
        .map(|issue| {
            let string = |field: &str| issue[field].as_str().map(str::to_string).ok_or_else(|| format!("board issue lacks {field}"));
            let blockers = issue["blockedBy"]["nodes"].as_array().ok_or("board lacks native dependencies")?;
            if issue["blockedBy"]["totalCount"].as_u64().is_none_or(|count| count != blockers.len() as u64) {
                return Err("board dependency window is truncated".into());
            }
            let blocked_by = blockers
                .iter()
                .map(|blocker| {
                    Ok(DispatchBoardDependency {
                        url: blocker["url"].as_str().ok_or("board blocker lacks URL")?.into(),
                        state: match blocker["state"].as_str() {
                            Some("OPEN") => IssueState::Open,
                            Some("CLOSED") => IssueState::Closed,
                            _ => return Err("board blocker lacks state".to_string()),
                        },
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let prs = issue["closedByPullRequestsReferences"].as_array().ok_or("board issue lacks PR references")?;
            if prs.len() >= 100 {
                return Err("board PR references may be truncated".into());
            }
            let pull_requests = prs
                .iter()
                .map(|pr| pr["url"].as_str().map(str::to_string).ok_or("board PR lacks URL".to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(DispatchBoardIssue::builder()
                .id(issue["number"].as_u64().ok_or("board issue lacks number")?.to_string())
                .title(string("title")?)
                .state(match string("state")?.as_str() {
                    "OPEN" => IssueState::Open,
                    "CLOSED" => IssueState::Closed,
                    _ => return Err("unknown issue state".into()),
                })
                .url(string("url")?)
                .updated_at(string("updatedAt")?)
                .maybe_issue_type(issue["issueType"]["name"].as_str().map(str::to_string))
                .maybe_parent(issue["parent"]["url"].as_str().map(crate::dispatch_missions::issue_ref_from_url).transpose()?)
                .maybe_closed_at(issue["closedAt"].as_str().map(str::to_string))
                .labels(
                    issue["labels"]
                        .as_array()
                        .ok_or("board issue lacks labels")?
                        .iter()
                        .filter_map(|label| label["name"].as_str().map(str::to_string))
                        .collect(),
                )
                .blocked_by(blocked_by)
                .pull_requests(pull_requests)
                .build())
        })
        .collect::<Result<Vec<_>, String>>()?;
    let pull_requests = prs
        .iter()
        .map(|pr| {
            let string = |field: &str| pr[field].as_str().map(str::to_string).ok_or_else(|| format!("board PR lacks {field}"));
            Ok(DispatchBoardPullRequest::builder()
                .id(pr["number"].as_u64().ok_or("board PR lacks number")?.to_string())
                .url(string("url")?)
                .state(string("state")?.to_ascii_lowercase())
                .maybe_merged_at(pr["mergedAt"].as_str().map(str::to_string))
                .maybe_merge_state(pr["mergeStateStatus"].as_str().map(str::to_ascii_lowercase))
                .ci(board_check_state(&pr["statusCheckRollup"]))
                .build())
        })
        .collect::<Result<Vec<_>, String>>()?;
    // Direct adapter observations have a completion timestamp; the daemon
    // cache owns serving age/error and stamps its own completed observation.
    Ok(DispatchBoardRepository {
        observed_at: Utc::now(),
        age_seconds: 0,
        refresh_error: None,
        source: source.clone(),
        issues,
        pull_requests,
    })
}

fn is_github_source(source: &IssueSource) -> bool {
    // Bare service names are valid for explicit Project issue-source overrides;
    // Forge-derived sources use the canonical URL form.
    matches!(source.service.trim_end_matches('/'), "github" | "github.com" | "https://github.com")
}

fn parse_issue(source: &IssueSource, v: &serde_json::Value, fetched_at: DateTime<Utc>) -> Option<Issue> {
    let number = v["number"].as_i64()?;
    let title = v["title"].as_str()?.to_string();
    let body = v["body"].as_str().map(str::to_string);
    let state = match v["state"].as_str()? {
        "open" => IssueState::Open,
        "closed" => IssueState::Closed,
        _ => return None,
    };
    let labels: Vec<String> = v["labels"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|l| l["name"].as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let assignees = v["assignees"]
        .as_array()
        .map(|items| items.iter().filter_map(|item| item["login"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let id = number.to_string();
    let as_of = v["updated_at"].as_str().and_then(|value| value.parse::<DateTime<Utc>>().ok()).unwrap_or(fetched_at);
    let reference = IssueRef { source: source.clone(), id: id.clone() };
    Some(
        Issue::builder()
            .reference(reference)
            .title(title)
            .maybe_body(body)
            .state(state)
            .labels(labels)
            .assignees(assignees)
            .as_of(as_of)
            .observed_at(fetched_at)
            .provider_name("github".into())
            .provider_display_name("GitHub".into())
            .build(),
    )
}

fn board_check_state(checks: &serde_json::Value) -> String {
    let Some(checks) = checks.as_array().filter(|checks| !checks.is_empty()) else { return "none".into() };
    let states = checks.iter().map(|check| check["status"].as_str().or_else(|| check["state"].as_str()).unwrap_or_default());
    if states.clone().any(|state| matches!(state, "EXPECTED" | "IN_PROGRESS" | "PENDING" | "QUEUED" | "REQUESTED" | "WAITING")) {
        return "pending".into();
    }
    let conclusions = checks.iter().map(|check| check["conclusion"].as_str().or_else(|| check["state"].as_str()).unwrap_or_default());
    if conclusions.clone().any(|state| !state.is_empty() && !matches!(state, "NEUTRAL" | "SKIPPED" | "SUCCESS")) {
        return "failure".into();
    }
    if conclusions.clone().all(|state| matches!(state, "NEUTRAL" | "SKIPPED" | "SUCCESS")) {
        "success".into()
    } else {
        "unknown".into()
    }
}

#[async_trait]
impl super::IssueProvider for GitHubIssueProvider {
    fn supports(&self, source: &IssueSource) -> bool {
        is_github_source(source)
    }

    async fn query(&self, source: &IssueSource, params: &IssueQuery, page: u32, count: usize) -> Result<IssueResultPage, String> {
        let per_page = clamp_per_page(count);
        let as_of = Utc::now();
        let (items, has_more, total) = match &params.search {
            None => {
                let label = params.label.as_ref().map(|label| format!("&labels={}", urlencoding::encode(label))).unwrap_or_default();
                let fields = params
                    .match_fields
                    .iter()
                    .map(|(field, values)| format!("&{}={}", urlencoding::encode(field), urlencoding::encode(&values.join(","))))
                    .collect::<String>();
                let endpoint = format!(
                    "repos/{}/issues?state=open&sort=updated&direction=desc&per_page={}&page={}{}{}",
                    source.scope, per_page, page, label, fields
                );
                let response = gh_api_get_with_headers!(self.api, &endpoint, &self.host_root)?;
                let raw_items: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|error| error.to_string())?;
                let issues = raw_items
                    .into_iter()
                    .filter(|value| !value.as_object().is_some_and(|object| object.contains_key("pull_request")))
                    .filter_map(|value| parse_issue(source, &value, as_of))
                    .collect();
                (issues, response.has_next_page, None)
            }
            Some(search_term) => {
                let label = params.label.as_ref().map(|label| format!(" label:\"{label}\"")).unwrap_or_default();
                let fields = params
                    .match_fields
                    .iter()
                    .flat_map(|(field, values)| values.iter().map(move |value| format!(" {field}:\"{value}\"")))
                    .collect::<String>();
                let raw_query = format!("repo:{} is:issue is:open{}{} {}", source.scope, label, fields, search_term);
                let encoded_query = urlencoding::encode(&raw_query);
                let endpoint = format!("search/issues?q={}&sort=updated&order=desc&per_page={}&page={}", encoded_query, per_page, page);
                let response = gh_api_get_with_headers!(self.api, &endpoint, &self.host_root)?;
                let parsed: serde_json::Value = serde_json::from_str(&response.body).map_err(|error| error.to_string())?;
                let total = parsed["total_count"].as_u64().and_then(|value| u32::try_from(value).ok());
                let raw_items = parsed["items"].as_array().ok_or("no items array in search response")?;
                let issues = raw_items.iter().filter_map(|value| parse_issue(source, value, as_of)).collect();
                (issues, response.has_next_page, total)
            }
        };
        Ok(IssueResultPage { items, total, has_more })
    }

    async fn fetch_by_id(&self, reference: &IssueRef) -> Result<Issue, String> {
        let endpoint = format!("repos/{}/issues/{}", reference.source.scope, reference.id);
        let body = gh_api_get!(self.api, &endpoint, &self.host_root)?;
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        parse_issue(&reference.source, &value, Utc::now()).ok_or_else(|| format!("failed to parse issue {}", reference.id))
    }

    async fn dispatch_board(&self, source: &IssueSource) -> Result<flotilla_protocol::DispatchBoardRepository, String> {
        self.poll_board(source).await
    }

    async fn mission_fields(&self, reference: &IssueRef) -> Result<flotilla_protocol::MissionFields, String> {
        use crate::providers::ChannelLabel;
        let mut values = Vec::new();
        for page in 1.. {
            let endpoint = format!("repos/{}/issues/{}/issue-field-values?per_page=100&page={page}", reference.source.scope, reference.id);
            let response = self.api.get_classified_response(&endpoint, &self.host_root, &ChannelLabel::GhApi(endpoint.clone())).await;
            let response = match response {
                Ok(response) => response,
                // GitHub documents unsupported/not present fields as 404/410. Authentication,
                // budgets and server errors remain unavailable evidence, never empty fields.
                Err(error) if page == 1 && error.response.as_ref().is_some_and(|r| matches!(r.status, 404 | 410)) => {
                    return Ok(Default::default())
                }
                Err(error) => return Err(error.error.to_string()),
            };
            let batch: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|error| error.to_string())?;
            let count = batch.len();
            values.extend(batch);
            if count < 100 {
                break;
            }
            if page >= 100 {
                return Err("mission field window is truncated".into());
            }
        }
        crate::dispatch_missions::parse_mission_fields(&values)
    }

    async fn dispatch_facts(&self, reference: &IssueRef) -> Result<flotilla_protocol::DispatchIssueFacts, String> {
        let (relations, issue) = self.issue_detail(reference, "closedByPullRequestsReferences").await?;
        let mut facts =
            flotilla_protocol::DispatchIssueFacts { issue_type: issue["type"]["name"].as_str().map(str::to_string), ..Default::default() };
        let mut page = 1;
        loop {
            let endpoint =
                format!("repos/{}/issues/{}/dependencies/blocked_by?per_page=100&page={page}", reference.source.scope, reference.id);
            let response = gh_api_get_with_headers!(self.api, &endpoint, &self.host_root)?;
            let blockers: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|error| error.to_string())?;
            for blocker in blockers {
                let url = blocker["html_url"].as_str().ok_or("native blocker lacks URL")?;
                let url = url::Url::parse(url).map_err(|error| error.to_string())?;
                let parts = url.path_segments().ok_or("native blocker lacks path")?.collect::<Vec<_>>();
                let [owner, repo, "issues", id] = parts.as_slice() else { return Err("invalid native blocker URL".into()) };
                facts.blockers.push(IssueRef {
                    source: IssueSource {
                        service: format!("https://{}", url.host_str().ok_or("blocker lacks host")?),
                        scope: format!("{owner}/{repo}"),
                    },
                    id: (*id).into(),
                });
            }
            if !response.has_next_page {
                break;
            }
            page += 1;
        }
        let prs = relations["closedByPullRequestsReferences"].as_array().ok_or("missing closing PR references")?;
        if prs.len() >= 100 {
            return Err("closing PR window may be truncated".into());
        }
        for pr in prs {
            let url = url::Url::parse(pr["url"].as_str().ok_or("closing PR lacks URL")?).map_err(|e| e.to_string())?;
            let parts = url.path_segments().ok_or("closing PR lacks path")?.collect::<Vec<_>>();
            let [owner, repo, "pull", id] = parts.as_slice() else {
                return Err("invalid closing PR URL".into());
            };
            let endpoint = format!("repos/{owner}/{repo}/pulls/{id}");
            let raw = gh_api_get!(self.api, &endpoint, &self.host_root)?;
            let pr: serde_json::Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
            facts.has_open_pull_request |= pr["state"] == "open";
            facts.landed |= pr["merged_at"].as_str().is_some();
        }
        Ok(facts)
    }

    async fn list_changed_since(&self, source: &IssueSource, since: &str, _count: usize) -> Result<IssueChangeset, String> {
        // Incremental pages include PRs. Use the REST maximum independently
        // of the demanded window size to avoid needless overflow reloads.
        let per_page = INCREMENTAL_PAGE_SIZE;
        // Keep the URI stable so the GhApi ETag cache can validate every poll.
        // Filter the sorted page locally and reload if relevant changes may
        // continue onto another page.
        let endpoint = format!("repos/{}/issues?state=all&sort=updated&direction=desc&per_page={}", source.scope, per_page);
        let response = gh_api_get_with_headers!(self.api, &endpoint, &self.host_root)?;
        // GhApi serves the cached 200 body on a 304. Reapply it against the
        // caller's cursor: a previous consumer may have failed before it
        // committed the changes and must be able to retry without another 200.
        let items: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
        let as_of = Utc::now();
        let since = since.parse::<DateTime<Utc>>().map_err(|error| format!("invalid issue refresh cursor: {error}"))?;
        let has_more = response.has_next_page
            && items.last().is_some_and(|item| {
                item["updated_at"].as_str().and_then(|value| value.parse::<DateTime<Utc>>().ok()).is_none_or(|updated| updated >= since)
            });

        let mut updated = Vec::new();
        let mut closed = Vec::new();

        for v in &items {
            if v["updated_at"].as_str().and_then(|value| value.parse::<DateTime<Utc>>().ok()).is_some_and(|updated| updated < since) {
                continue;
            }
            if v.as_object().map(|o| o.contains_key("pull_request")).unwrap_or(false) {
                continue;
            }
            let state = v["state"].as_str().unwrap_or("open");
            if state == "open" {
                if let Some(issue) = parse_issue(source, v, as_of) {
                    updated.push(issue);
                }
            } else if let Some(number) = v["number"].as_i64() {
                closed.push(IssueRef { source: source.clone(), id: number.to_string() });
            }
        }

        Ok(IssueChangeset { updated, closed, has_more })
    }

    async fn open_in_browser(&self, reference: &IssueRef) -> Result<(), String> {
        run!(self.runner, "gh", &["issue", "view", &reference.id, "--repo", &reference.source.scope, "--web"], &self.host_root)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    use super::*;
    use crate::providers::{
        github_api::{GhApi, GhApiClient, GhApiResponse},
        github_test_support::{build_api_and_runner, repo_root_for_recording},
        issue_tracker::{tests::assert_provider_contract, IssueProvider},
        replay::{self, Masks},
        testing::{fixture_path, MockRunner},
        ChannelLabel,
    };

    #[test]
    fn parses_issue_title_and_assignees_from_existing_rest_response() {
        let source = IssueSource { service: "github.com".into(), scope: "team/repo".into() };
        let observed_at = "2026-10-01T12:00:00Z".parse().expect("time");
        let raw =
            serde_json::json!({"number": 7, "title": "Fix refresh", "state": "open", "assignees": [{"login": "alice"}, {"login": "bob"}]});
        let issue = parse_issue(&source, &raw, observed_at).expect("parse issue");
        assert_eq!(issue.title, "Fix refresh");
        assert_eq!(issue.assignees, ["alice", "bob"]);
        assert_eq!(issue.observed_at, Some(observed_at));
    }
    struct CountingGhApi {
        inner: Arc<dyn GhApi>,
        requests: AtomicUsize,
    }

    #[async_trait]
    impl GhApi for CountingGhApi {
        async fn get(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.get_with_headers(endpoint, repo_root, label).await.map(|response| response.body)
        }
        async fn get_with_headers(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<GhApiResponse, String> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.inner.get_with_headers(endpoint, repo_root, label).await
        }
    }

    struct MockGhApi {
        responses: Mutex<VecDeque<Result<GhApiResponse, String>>>,
        requests: Mutex<Vec<(String, PathBuf)>>,
    }

    impl MockGhApi {
        fn new(responses: Vec<Result<GhApiResponse, String>>) -> Self {
            Self { responses: Mutex::new(responses.into()), requests: Mutex::new(Vec::new()) }
        }

        fn requests(&self) -> Vec<(String, PathBuf)> {
            self.requests.lock().expect("GitHub request lock poisoned").clone()
        }
    }

    #[async_trait]
    impl GhApi for MockGhApi {
        async fn get(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.get_with_headers(endpoint, repo_root, label).await.map(|r| r.body)
        }
        async fn get_with_headers(&self, endpoint: &str, repo_root: &Path, _label: &ChannelLabel) -> Result<GhApiResponse, String> {
            self.requests.lock().expect("GitHub request lock poisoned").push((endpoint.to_string(), repo_root.to_path_buf()));
            self.responses.lock().expect("GitHub response lock poisoned").pop_front().expect("MockGhApi: no more responses")
        }
    }

    fn source() -> IssueSource {
        IssueSource { service: "https://github.com".into(), scope: "owner/repo".into() }
    }

    fn ok_response(body: &str, has_next_page: bool) -> Result<GhApiResponse, String> {
        Ok(GhApiResponse { status: 200, etag: None, body: body.to_string(), has_next_page, total_count: None })
    }

    fn mock_provider(responses: Vec<Result<GhApiResponse, String>>) -> GitHubIssueProvider {
        let api = Arc::new(MockGhApi::new(responses));
        let runner = Arc::new(MockRunner::new(vec![]));
        GitHubIssueProvider::new(api, runner, Path::new("/neutral"))
    }

    fn fixture(name: &str) -> String {
        fixture_path("issue_tracker", name)
    }

    #[tokio::test]
    async fn record_replay_satisfies_provider_contract() {
        let session = replay::test_session(&fixture("github_issues.yaml"), Masks::new());
        let repo_root = if session.is_live() { repo_root_for_recording() } else { PathBuf::from("/test/repo") };
        let (api, runner) = build_api_and_runner(&session);
        let source = IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() };
        let provider = GitHubIssueProvider::new(api, runner, repo_root);

        assert_provider_contract(&provider, &source, "747", "2026-07-01T00:00:00Z").await;

        session.finish();
    }

    #[tokio::test]
    async fn recorded_live_page_comparison_reduces_two_window_refresh_from_three_requests_to_one() {
        let session = replay::test_session(&fixture("github_refresh_request_counts.yaml"), Masks::new());
        let api = Arc::new(CountingGhApi { inner: replay::test_gh_api(&session), requests: Default::default() });
        let runner = replay::test_runner(&session);
        let provider = GitHubIssueProvider::new(api.clone(), runner, Path::new("/"));
        let source = IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() };
        let endpoint = format!("repos/{}/issues?state=all&sort=updated&direction=desc&per_page=100", source.scope);
        // Choose a reproducible cutoff from the recorded live page. This setup
        // request is excluded from each measured refresh path.
        let sample = gh_api_get_with_headers!(api, &endpoint, Path::new("/")).expect("live sample");
        let records: Vec<serde_json::Value> = serde_json::from_str(&sample.body).expect("records");
        let since = records[70]["updated_at"].as_str().expect("cutoff");
        assert!(records.iter().any(|record| record.get("pull_request").is_some()), "sample must include PR traffic");
        api.requests.store(0, Ordering::Relaxed);
        // Reproduce the previous 50-record endpoint and its conservative full
        // reload of both demanded windows when the cutoff extends past it.
        let endpoint = format!("repos/{}/issues?state=all&sort=updated&direction=desc&per_page=50", source.scope);
        let before = gh_api_get_with_headers!(api, &endpoint, Path::new("/")).expect("previous page");
        let before_records: Vec<serde_json::Value> = serde_json::from_str(&before.body).expect("previous records");
        assert!(before.has_next_page && before_records.last().expect("last record")["updated_at"].as_str().expect("updated") >= since);
        for _ in 0..2 {
            provider.query(&source, &IssueQuery::default(), 1, 50).await.expect("previous window reload");
        }
        assert_eq!(api.requests.load(Ordering::Relaxed), 3);
        api.requests.store(0, Ordering::Relaxed);
        let after = provider.list_changed_since(&source, since, 50).await.expect("larger incremental page");
        assert!(!after.has_more, "the maximum page contains the complete refresh interval");
        assert_eq!(api.requests.load(Ordering::Relaxed), 1);
        for record in records
            .iter()
            .filter(|record| record.get("pull_request").is_none() && record["updated_at"].as_str().is_some_and(|updated| updated >= since))
        {
            let id = record["number"].as_u64().expect("number").to_string();
            assert!(
                after.updated.iter().any(|issue| issue.reference.id == id) || after.closed.iter().any(|reference| reference.id == id),
                "missed issue {id}"
            );
        }
        session.finish();
    }

    #[tokio::test]
    async fn fetch_by_id_uses_source_identity_without_a_checkout() {
        let before = Utc::now();
        let api = Arc::new(MockGhApi::new(vec![ok_response(
            r#"{"number":42,"title":"The answer","body":"Details","state":"closed","labels":[{"name":"bug"}],"updated_at":"2026-07-20T12:34:56Z"}"#,
            false,
        )]));
        let provider = GitHubIssueProvider::new(api.clone(), Arc::new(MockRunner::new(vec![])), Path::new("/host-capability"));
        let reference = IssueRef { source: source(), id: "42".into() };

        let issue = provider.fetch_by_id(&reference).await.expect("fetch-by-id should succeed");

        assert_eq!(issue.reference, reference);
        assert_eq!(issue.body.as_deref(), Some("Details"));
        assert_eq!(issue.state, IssueState::Closed);
        assert_eq!(issue.as_of, "2026-07-20T12:34:56Z".parse::<DateTime<Utc>>().expect("timestamp"));
        assert!(issue.observed_at.is_some_and(|observed_at| observed_at >= before && observed_at <= Utc::now()));
        assert_eq!(api.requests(), vec![("repos/owner/repo/issues/42".into(), PathBuf::from("/host-capability"))]);
    }

    #[tokio::test]
    async fn query_filters_pull_requests_and_preserves_source_refs() {
        let body = r#"[
            {"number":1,"title":"Real issue","state":"open","labels":[]},
            {"number":2,"title":"A PR","state":"open","labels":[],"pull_request":{"url":"..."}},
            {"number":3,"title":"Another issue","state":"open","labels":[]}
        ]"#;
        let provider = mock_provider(vec![ok_response(body, false)]);

        let page = provider.query(&source(), &IssueQuery::default(), 1, 10).await.expect("issue query should succeed");

        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].reference, IssueRef { source: source(), id: "1".into() });
        assert_eq!(page.items[1].reference.id, "3");
    }

    #[tokio::test]
    async fn query_sends_label_filter_to_github() {
        let api = Arc::new(MockGhApi::new(vec![ok_response("[]", false)]));
        let provider = GitHubIssueProvider::new(api.clone(), Arc::new(MockRunner::new(vec![])), Path::new("/neutral"));

        provider
            .query(&source(), &IssueQuery { search: None, label: Some("ready now".into()), match_fields: Default::default() }, 2, 10)
            .await
            .expect("label-filtered issue query should succeed");

        assert_eq!(
            api.requests(),
            vec![(
                "repos/owner/repo/issues?state=open&sort=updated&direction=desc&per_page=10&page=2&labels=ready%20now".into(),
                PathBuf::from("/neutral"),
            )]
        );
    }

    #[tokio::test]
    async fn search_query_returns_total_and_pagination() {
        let body = r#"{"total_count":5,"items":[{"number":1,"title":"Bug","state":"open","labels":[]}]}"#;
        let provider = mock_provider(vec![ok_response(body, true)]);

        let page = provider
            .query(&source(), &IssueQuery { search: Some("bug".into()), label: None, match_fields: Default::default() }, 2, 10)
            .await
            .expect("issue search should succeed");

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.total, Some(5));
        assert!(page.has_more);
    }

    #[tokio::test]
    async fn open_in_browser_passes_source_scope_explicitly() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let provider = GitHubIssueProvider::new(Arc::new(MockGhApi::new(vec![])), runner.clone(), Path::new("/neutral"));

        provider.open_in_browser(&IssueRef { source: source(), id: "42".into() }).await.expect("open-in-browser should succeed");

        assert_eq!(
            runner.calls(),
            vec![("gh".into(), vec!["issue", "view", "42", "--repo", "owner/repo", "--web"].into_iter().map(str::to_string).collect())]
        );
    }

    #[tokio::test]
    async fn changed_since_partitions_open_and_closed() {
        let body = r#"[
            {"number": 1, "title": "Open issue", "state": "open", "labels": []},
            {"number": 2, "title": "Closed issue", "state": "closed", "labels": []},
            {"number": 3, "title": "Another open", "state": "open", "labels": []}
        ]"#;
        let provider = mock_provider(vec![Ok(GhApiResponse {
            status: 200,
            etag: None,
            body: body.to_string(),
            has_next_page: false,
            total_count: None,
        })]);

        let changeset =
            provider.list_changed_since(&source(), "2026-03-09T00:00:00Z", 50).await.expect("changed-since query should succeed");

        assert_eq!(changeset.updated.len(), 2);
        assert_eq!(changeset.updated[0].reference.id, "1");
        assert_eq!(changeset.updated[1].reference.id, "3");
        assert_eq!(changeset.closed, vec![IssueRef { source: source(), id: "2".into() }]);
        assert!(!changeset.has_more);
    }

    #[tokio::test]
    async fn incremental_poll_reuses_etag_and_304_spends_no_primary_quota() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("HTTP/2 200 OK\r\nETag: \"issue-window\"\r\nX-RateLimit-Remaining: 4800\r\n\r\n[{\"number\":1,\"title\":\"Changed\",\"state\":\"open\",\"labels\":[],\"updated_at\":\"2026-07-01T00:00:10Z\"}]".into()),
            Ok("HTTP/2 304 Not Modified\r\nETag: \"issue-window\"\r\nX-RateLimit-Remaining: 4800\r\n\r\n".into()),
        ]));
        let api = Arc::new(GhApiClient::new(runner.clone()));
        let provider = GitHubIssueProvider::new(api, runner.clone(), Path::new("/neutral"));

        let first = provider.list_changed_since(&source(), "2026-07-01T00:00:00Z", 50).await.expect("first poll");
        let second = provider.list_changed_since(&source(), "2026-07-01T00:00:20Z", 50).await.expect("conditional poll");

        assert_eq!(first.updated.len(), 1);
        assert!(second.updated.is_empty());
        assert!(second.closed.is_empty());
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1[2], calls[1].1[2], "incremental endpoint must be stable for ETag reuse");
        assert!(calls[1].1.contains(&"If-None-Match: \"issue-window\"".to_string()));
    }

    #[tokio::test]
    async fn conditional_hit_replays_a_change_when_the_consumer_retries_its_cursor() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("HTTP/2 200 OK\r\nETag: \"issue-window\"\r\n\r\n[{\"number\":1,\"title\":\"Changed\",\"state\":\"open\",\"labels\":[],\"updated_at\":\"2026-07-01T00:00:10Z\"}]".into()),
            Ok("HTTP/2 304 Not Modified\r\nETag: \"issue-window\"\r\n\r\n".into()),
        ]));
        let api = Arc::new(GhApiClient::new(runner.clone()));
        let provider = GitHubIssueProvider::new(api, runner, Path::new("/neutral"));

        provider.list_changed_since(&source(), "2026-07-01T00:00:00Z", 50).await.expect("first poll");
        let retried = provider.list_changed_since(&source(), "2026-07-01T00:00:00Z", 50).await.expect("retry after 304");

        assert_eq!(retried.updated.len(), 1, "a 304 must not discard uncommitted changes");
    }

    #[tokio::test]
    async fn pr_heavy_refresh_uses_the_largest_stable_page_without_reloading() {
        let mut items = (1..=50)
            .map(|number| {
                serde_json::json!({"number": number,
            "title": "PR", "state": "open", "pull_request": {}, "updated_at": "2026-07-01T00:00:10Z"})
            })
            .collect::<Vec<_>>();
        items.push(serde_json::json!({"number": 51, "title": "Issue", "state": "open",
            "updated_at": "2026-07-01T00:00:05Z"}));
        let api = Arc::new(MockGhApi::new(vec![ok_response(&serde_json::to_string(&items).expect("items"), false)]));
        let provider = GitHubIssueProvider::new(api.clone(), Arc::new(MockRunner::new(vec![])), Path::new("/neutral"));
        let changes = provider.list_changed_since(&source(), "2026-07-01T00:00:00Z", 50).await.expect("refresh");
        assert_eq!(changes.updated[0].reference.id, "51");
        assert!(!changes.has_more, "the issue behind fifty PRs must not require a reload");
        assert_eq!(api.requests().len(), 1);
        assert!(api.requests()[0].0.ends_with("per_page=100"), "use GitHub's maximum page size independently of query window size");
    }

    #[tokio::test]
    async fn changed_since_filters_pull_requests() {
        let body = r#"[
            {"number": 1, "title": "Issue", "state": "open", "labels": []},
            {"number": 2, "title": "PR", "state": "open", "labels": [], "pull_request": {"url": "..."}}
        ]"#;
        let provider = mock_provider(vec![Ok(GhApiResponse {
            status: 200,
            etag: None,
            body: body.to_string(),
            has_next_page: false,
            total_count: None,
        })]);

        let changeset =
            provider.list_changed_since(&source(), "2026-03-09T00:00:00Z", 50).await.expect("changed-since query should succeed");

        assert_eq!(changeset.updated.len(), 1);
        assert_eq!(changeset.updated[0].reference.id, "1");
        assert!(changeset.closed.is_empty());
    }

    #[tokio::test]
    async fn changed_since_escalates_on_pr_only_page_with_more_raw_results() {
        let body = r#"[
            {"number": 10, "title": "PR A", "state": "open", "labels": [], "pull_request": {"url": "..."}},
            {"number": 11, "title": "PR B", "state": "open", "labels": [], "pull_request": {"url": "..."}}
        ]"#;
        let provider = mock_provider(vec![Ok(GhApiResponse {
            status: 200,
            etag: None,
            body: body.to_string(),
            has_next_page: true,
            total_count: None,
        })]);

        let changeset =
            provider.list_changed_since(&source(), "2026-03-09T00:00:00Z", 2).await.expect("changed-since query should succeed");

        assert!(changeset.updated.is_empty());
        assert!(changeset.closed.is_empty());
        assert!(changeset.has_more, "later raw pages may contain issue changes");
    }

    #[tokio::test]
    async fn changed_since_escalates_on_mixed_pr_issue_page() {
        // Page has both PRs and issues with has_next_page — should escalate
        // because remaining pages may contain more issues.
        let body = r#"[
            {"number": 1, "title": "Issue", "state": "open", "labels": []},
            {"number": 2, "title": "PR", "state": "open", "labels": [], "pull_request": {"url": "..."}}
        ]"#;
        let provider = mock_provider(vec![Ok(GhApiResponse {
            status: 200,
            etag: None,
            body: body.to_string(),
            has_next_page: true,
            total_count: None,
        })]);

        let changeset =
            provider.list_changed_since(&source(), "2026-03-09T00:00:00Z", 2).await.expect("changed-since query should succeed");

        assert_eq!(changeset.updated.len(), 1);
        assert!(changeset.has_more, "should escalate when page has issues and more pages exist");
    }
    // Authenticated native dependency and closing-PR REST evidence.
    #[tokio::test]
    async fn dispatch_facts_record_replay() {
        let session = replay::test_session(&fixture("github_dispatch_facts.yaml"), Masks::new());
        let (api, runner) = build_api_and_runner(&session);
        let provider = GitHubIssueProvider::new(api, runner, Path::new("/"));
        let source = IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() };
        let dependent = provider.dispatch_facts(&IssueRef { source: source.clone(), id: "2783".into() }).await.expect("native facts");
        assert!(dependent.blockers.contains(&IssueRef { source: source.clone(), id: "2782".into() }));
        assert_eq!(dependent.issue_type.as_deref(), Some("Task"));
        let landed = provider.dispatch_facts(&IssueRef { source, id: "1385".into() }).await.expect("landing facts");
        assert!(landed.landed);
        assert!(!landed.has_open_pull_request);
        session.finish();
    }

    // The authenticated REST field request is recorded at the process seam;
    // pure normalization tests cover the documented numeric/select payloads.
    #[tokio::test]
    async fn mission_fields_record_replay() {
        let session = replay::test_session(&fixture("github_mission_fields.yaml"), Masks::new());
        let (api, runner) = build_api_and_runner(&session);
        let provider = GitHubIssueProvider::new(api, runner, Path::new("/"));
        let fields = provider
            .mission_fields(&IssueRef {
                source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() },
                id: "2783".into(),
            })
            .await
            .expect("mission fields or documented unsupported fallback");
        assert_eq!(fields, flotilla_protocol::MissionFields::default());
        session.finish();
    }

    // #2868: unchanged native details survive rediscovery, but a new item ETag
    // invalidates them even when GitHub's second-resolution updated_at is equal.
    #[tokio::test]
    async fn issue_detail_etag_invalidates_equal_timestamp_cache() {
        let item = serde_json::json!({"number":7,"updated_at":"2026-10-07T00:00:00Z"});
        let runner = Arc::new(MockRunner::new(vec![
            Ok(format!("HTTP/2 200 OK\r\nETag: initial\r\n\r\n{item}")),
            Ok(r#"{"closedByPullRequestsReferences":[]}"#.into()),
            Ok("HTTP/2 304 Not Modified\r\n\r\n".into()),
            Ok(format!("HTTP/2 200 OK\r\nETag: changed\r\n\r\n{item}")),
            Ok(r#"{"closedByPullRequestsReferences":[{"number":8}]}"#.into()),
        ]));
        let directory = tempfile::tempdir().expect("persistent native details");
        let make = || {
            GitHubIssueProvider::new(
                Arc::new(GhApiClient::new(runner.clone()).with_persistence(directory.path().join("rest"))),
                runner.clone(),
                Path::new("/"),
            )
            .with_poll_directory(directory.path().join("details"))
        };
        let reference = IssueRef { source: source(), id: "7".into() };
        let provider = make();
        let (first, _) = provider.issue_detail(&reference, "closedByPullRequestsReferences").await.expect("initial");
        drop(provider);
        let provider = make();
        assert_eq!(provider.issue_detail(&reference, "closedByPullRequestsReferences").await.expect("quiet").0, first);
        let (changed, _) = provider.issue_detail(&reference, "closedByPullRequestsReferences").await.expect("changed ETag");
        assert_eq!(changed["closedByPullRequestsReferences"][0]["number"], 8);
        assert_eq!(runner.calls().len(), 5);
        assert_eq!(runner.remaining(), 0);
    }

    // Historical full-board replay is replaced by conditional-poll tests: the
    // production adapter now validates REST collections before targeted details.
    #[tokio::test]
    async fn board_quiet_poll_restart_and_single_revision_only_fetch_changed_details() {
        fn response(etag: &str, body: serde_json::Value) -> String {
            format!("HTTP/2 200 OK\r\nETag: {etag}\r\n\r\n{body}")
        }
        fn issue(revision: &str) -> serde_json::Value {
            serde_json::json!({"number": 7, "updated_at": revision})
        }
        fn detail(title: &str) -> String {
            serde_json::json!({"number":7,"title":title,"state":"OPEN","url":"https://github.com/team/repo/issues/7",
                "updatedAt":"2026-10-07T00:00:00Z","closedAt":null,"labels":[],"blockedBy":{"totalCount":0,"nodes":[]},
                "closedByPullRequestsReferences":[],"parent":null,"issueType":{"name":"Task"}})
            .to_string()
        }
        let unchanged = "HTTP/2 304 Not Modified\r\n\r\n".to_string();
        let runner = Arc::new(MockRunner::new(vec![
            Ok(response("issues-1", serde_json::json!([issue("2026-10-07T00:00:00Z")]))),
            Ok(response("pulls-1", serde_json::json!([]))),
            Ok(detail("Initial")),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(response("issues-2", serde_json::json!([issue("2026-10-07T00:01:00Z")]))),
            Ok(response("delta-2", serde_json::json!([issue("2026-10-07T00:01:00Z")]))),
            Ok(unchanged.clone()),
            Ok(detail("Changed")),
            Ok(response("issues-3", serde_json::json!([issue("2026-10-07T00:02:00Z")]))),
            Ok(response("delta-3", serde_json::json!([issue("2026-10-07T00:02:00Z")]))),
            Ok(unchanged.clone()),
            Ok("{}".into()),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(detail("Recovered")),
        ]));
        let directory = tempfile::tempdir().expect("poll directory");
        let budgets = crate::forge_budget::ForgeBudgets::default();
        let metered = Arc::new(crate::forge_budget::BudgetedRunner { inner: runner.clone(), budgets: budgets.clone() });
        let make = || {
            let api = Arc::new(GhApiClient::new(metered.clone()).with_persistence(directory.path().join("rest")));
            GitHubIssueProvider::new(api, metered.clone(), Path::new("/")).with_poll_directory(directory.path().join("board"))
        };
        let source = IssueSource { service: "https://github.com".into(), scope: "team/repo".into() };
        let provider = make();
        assert_eq!(provider.dispatch_board(&source).await.expect("initial").issues[0].title, "Initial");
        assert_eq!(runner.calls().len(), 3);
        assert_eq!(provider.dispatch_board(&source).await.expect("quiet").issues[0].title, "Initial");
        drop(provider);
        let restarted = make();
        assert_eq!(restarted.dispatch_board(&source).await.expect("restart").issues[0].title, "Initial");
        assert_eq!(runner.calls().len(), 7);
        let rest = budgets.rows("test").into_iter().find(|row| row.budget == "REST").expect("REST budget");
        assert_eq!((rest.calls, rest.reported_cost), (6, 2), "quiet and restart polls spend zero primary quota");
        assert_eq!(restarted.dispatch_board(&source).await.expect("single update").issues[0].title, "Changed");
        let calls = runner.calls();
        assert_eq!(calls.len(), 11);
        assert_eq!(calls.iter().filter(|(_, args)| args.first().is_some_and(|arg| arg == "issue")).count(), 2);
        for (_, args) in &calls[3..7] {
            assert_eq!(args[0], "api");
            assert!(args.iter().any(|arg| arg.starts_with("If-None-Match:")), "quiet requests must be conditional: {args:?}");
        }
        assert!(calls.iter().all(|(_, args)| !args.contains(&"list".to_string())));
        assert!(restarted.dispatch_board(&source).await.is_err(), "incomplete native detail must not commit a cursor");
        assert_eq!(restarted.poll.load(&source.scope).unwrap().issues.cursor.as_deref(), Some("2026-10-07T00:01:00Z"));
        assert_eq!(restarted.dispatch_board(&source).await.expect("retry cached collection").issues[0].title, "Recovered");
        assert_eq!(runner.calls().len(), 18);
        assert_eq!(runner.remaining(), 0);
    }
    // The archived full-board recording pins the same per-item native shape
    // consumed by targeted detail reads. Production never uses these list calls.
    #[tokio::test]
    async fn historical_board_detail_decoder_record_replay() {
        let session = replay::test_session(&fixture("github_dispatch_board.yaml"), Masks::new());
        let (_api, runner) = build_api_and_runner(&session);
        let source = IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() };
        let issues = run!(
            runner,
            "gh",
            &[
                "issue",
                "list",
                "--repo",
                &source.scope,
                "--state",
                "all",
                "--limit",
                "10000",
                "--json",
                "number,title,state,url,updatedAt,closedAt,labels,blockedBy,closedByPullRequestsReferences,parent,issueType"
            ],
            Path::new("/")
        )
        .expect("archived issue detail shapes");
        let pulls = run!(
            runner,
            "gh",
            &[
                "pr",
                "list",
                "--repo",
                &source.scope,
                "--state",
                "all",
                "--limit",
                "10000",
                "--json",
                "number,state,url,mergedAt,mergeStateStatus,statusCheckRollup"
            ],
            Path::new("/")
        )
        .expect("archived PR detail shapes");
        let board = parse_board(&source, serde_json::from_str(&issues).unwrap(), serde_json::from_str(&pulls).unwrap()).unwrap();
        let dependent = board.issues.iter().find(|issue| issue.id == "2783").expect("dependent");
        assert_eq!(dependent.issue_type.as_deref(), Some("Task"));
        assert!(board.issues.iter().any(|issue| issue.parent.is_some()));
        assert!(dependent.blocked_by.iter().any(|blocker| blocker.url.ends_with("/issues/2782")));
        let landed = board.pull_requests.iter().find(|pr| pr.id == "1387").expect("landed PR");
        assert_eq!(landed.state, "merged");
        assert!(landed.merged_at.is_some());
        session.finish();
    }
}
