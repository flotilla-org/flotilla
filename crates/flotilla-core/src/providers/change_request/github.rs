use crate::providers::github_poll::PollCache;

use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;

use crate::providers::{
    change_request::observation::{parse_gh_observation_value_with_crew_identity, DEFAULT_REVIEW_BOT_LOGIN},
    forge::{
        github::{clamp_per_page, GhApi},
        observation_error::ObservationError,
    },
    gh_api_channel_label, gh_api_get,
    github_observation::{ObservationTelemetry, ParsedObservationResponse, QueryShape},
    run, run_output,
    types::*,
    CommandRunner,
};

fn execution_root() -> &'static Path {
    Path::new("/")
}

const MAX_HISTORY_PAGE_QUERIES: usize = 8;
const MAX_HISTORY_PAGE_NODES: usize = 800;
const HISTORY_PAGE_SIZE: usize = 100;
const THREAD_PAGE_SIZE: usize = 20;
const THREAD_COMMENT_PREVIEW_SIZE: usize = 10;

enum HistoryPage {
    Comments,
    Reviews,
    Threads,
    ThreadComments(usize),
}

impl HistoryPage {
    fn connection<'a>(&self, request: &'a serde_json::Value) -> &'a serde_json::Value {
        match self {
            Self::Comments => &request["comments"],
            Self::Reviews => &request["reviews"],
            Self::Threads => &request["reviewThreads"],
            Self::ThreadComments(index) => &request["reviewThreads"]["nodes"][*index]["comments"],
        }
    }

    fn connection_mut<'a>(&self, request: &'a mut serde_json::Value) -> &'a mut serde_json::Value {
        match self {
            Self::Comments => &mut request["comments"],
            Self::Reviews => &mut request["reviews"],
            Self::Threads => &mut request["reviewThreads"],
            Self::ThreadComments(index) => &mut request["reviewThreads"]["nodes"][*index]["comments"],
        }
    }

    fn max_nodes(&self) -> usize {
        match self {
            Self::Threads => THREAD_PAGE_SIZE * (1 + THREAD_COMMENT_PREVIEW_SIZE),
            _ => HISTORY_PAGE_SIZE,
        }
    }

    fn field(&self) -> &'static str {
        match self {
            Self::Comments => "comments",
            Self::Reviews => "reviews",
            Self::Threads => "reviewThreads",
            Self::ThreadComments(_) => "comments",
        }
    }

    fn selection(&self, request: &serde_json::Value, number: u64, cursor: &str) -> Result<String, String> {
        Ok(match self {
            Self::Comments => format!("comments(last:{HISTORY_PAGE_SIZE},before:{cursor}) {{ pageInfo {{ hasPreviousPage startCursor }} nodes {{ databaseId createdAt author {{ login __typename }} body }} }}"),
            Self::Reviews => format!("reviews(last:{HISTORY_PAGE_SIZE},before:{cursor}) {{ pageInfo {{ hasPreviousPage startCursor }} nodes {{ fullDatabaseId submittedAt author {{ login __typename }} state body }} }}"),
            Self::Threads => format!("reviewThreads(last:{THREAD_PAGE_SIZE},before:{cursor}) {{ pageInfo {{ hasPreviousPage startCursor }} nodes {{ id isResolved comments(last:{THREAD_COMMENT_PREVIEW_SIZE}) {{ pageInfo {{ hasPreviousPage startCursor }} nodes {{ fullDatabaseId createdAt author {{ login __typename }} body }} }} }} }}"),
            Self::ThreadComments(index) => {
                let id = request["reviewThreads"]["nodes"][*index]["id"]
                    .as_str()
                    .ok_or_else(|| format!("change request {number} truncated thread has no id"))?;
                let id = serde_json::to_string(id).map_err(|error| error.to_string())?;
                format!("node(id:{id}) {{ ... on PullRequestReviewThread {{ comments(last:{HISTORY_PAGE_SIZE},before:{cursor}) {{ pageInfo {{ hasPreviousPage startCursor }} nodes {{ fullDatabaseId createdAt author {{ login __typename }} body }} }} }} }}")
            }
        })
    }
}

pub struct GitHubChangeRequest {
    provider_name: String,
    repo_slug: String,
    api: Arc<dyn GhApi>,
    runner: Arc<dyn CommandRunner>,
    review_bot_login: String,
    operator_login: Option<String>,
    poll: Option<PollCache>,
}

#[derive(Debug, bon::Builder)]
struct GhPr {
    number: i64,
    title: String,
    head_ref_name: String,
    base_ref_name: Option<String>,
    state: String,
    body: Option<String>,
    is_draft: bool,
    merged_at: Option<String>,
}

impl GitHubChangeRequest {
    pub fn new(provider_name: String, repo_slug: String, api: Arc<dyn GhApi>, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            provider_name,
            repo_slug,
            api,
            runner,
            review_bot_login: DEFAULT_REVIEW_BOT_LOGIN.to_string(),
            operator_login: None,
            poll: None,
        }
    }

    pub fn with_poll_directory(mut self, directory: std::path::PathBuf) -> Self {
        self.poll = Some(PollCache::default().with_directory(directory));
        self
    }

    pub fn with_review_bot_login(mut self, login: String) -> Self {
        self.review_bot_login = login;
        self
    }

    pub fn with_operator_login(mut self, login: String) -> Self {
        self.operator_login = Some(login);
        self
    }

    async fn poll_pull_requests(&self) -> Result<Vec<serde_json::Value>, String> {
        let poll = self.poll.as_ref().ok_or("durable PR observer is not configured")?;
        let mut cache = poll.state.lock().await;
        let key = format!("{}/pulls", self.repo_slug);
        let state = cache.entry(key.clone()).or_insert(poll.load(&key)?);
        let mut next = state.clone();
        for item in next.pulls.changes(self.api.as_ref(), execution_root(), &self.repo_slug, "pulls").await? {
            next.pulls.commit(&item, item.clone())?;
        }
        poll.save(&key, &next)?;
        let items = next.pulls.details.values().cloned().collect();
        *state = next;
        Ok(items)
    }

    async fn observe_changed(
        &self,
        numbers: &[u64],
        crew_logins: &super::CrewGithubLoginsByRequest,
    ) -> Result<super::BoundObservations, ObservationError> {
        if numbers.is_empty() {
            return Ok(HashMap::new());
        }
        let (owner, name) = self.repo_slug.split_once('/').ok_or("GitHub repository must have owner/name scope")?;
        let owner = serde_json::to_string(owner).map_err(|error| error.to_string())?;
        let name = serde_json::to_string(name).map_err(|error| error.to_string())?;
        let mut query = format!("query {{ rateLimit {{ cost }} repository(owner:{owner}, name:{name}) {{");
        // Keep every bound CR in one repository query so request count is
        // independent of convoy count. If a repository exceeds GitHub's query
        // limits, surface the forge error instead of silently omitting CRs.
        for number in numbers {
            query.push_str(&format!(" pr{number}: pullRequest(number:{number}) {{ title state isDraft headRefOid reviewDecision mergeable author {{ login }} reviewRequests(first:100) {{ pageInfo {{ hasNextPage }} nodes {{ requestedReviewer {{ __typename ... on User {{ login }} }} }} }} comments(last:100) {{ pageInfo {{ hasPreviousPage startCursor }} nodes {{ databaseId createdAt author {{ login __typename }} body }} }} reviews(last:100) {{ pageInfo {{ hasPreviousPage startCursor }} nodes {{ fullDatabaseId submittedAt author {{ login __typename }} state body }} }} reviewThreads(last:20) {{ pageInfo {{ hasPreviousPage startCursor }} nodes {{ id isResolved comments(last:10) {{ pageInfo {{ hasPreviousPage startCursor }} nodes {{ fullDatabaseId createdAt author {{ login __typename }} body }} }} }} }} timelineItems(last:1,itemTypes:[HEAD_REF_FORCE_PUSHED_EVENT]) {{ nodes {{ ... on HeadRefForcePushedEvent {{ createdAt afterCommit {{ oid }} }} }} }} commits(last:1) {{ nodes {{ commit {{ committedDate pushedDate statusCheckRollup {{ contexts(first:100) {{ nodes {{ ... on CheckRun {{ conclusion status }} ... on StatusContext {{ state }} }} }} }} }} }} }} }}"));
        }
        query.push_str(" } }");
        let mut telemetry = ObservationTelemetry::new(&self.repo_slug, numbers.len());
        let output = self.observation_call(&query, QueryShape::BoundBatch, numbers.len(), &mut telemetry).await?;
        let success = output.success;
        let stderr = output.stderr.clone();
        let document = output.into_document()?;
        if let Some(errors) = document["errors"].as_array() {
            let unexpected = errors.iter().filter(|error| error["type"] != "NOT_FOUND").collect::<Vec<_>>();
            if !unexpected.is_empty() {
                return Err(format!("GitHub GraphQL observation: {unexpected:?}").into());
            }
        } else if !success {
            return Err(format!("GitHub GraphQL observation failed: {stderr}").into());
        }
        let repository = &document["data"]["repository"];
        if !repository.is_object() {
            return Err(format!("GitHub GraphQL repository {} unavailable", self.repo_slug).into());
        }
        let observed_at = Utc::now();
        let mut statuses = HashMap::new();
        let (mut pages, mut nodes) = (0, 0);
        let mut cooldown: Option<ObservationError> = None;
        // The caller's order determines which PR receives history pagination
        // priority when the shared follow-up budget is exhausted.
        for number in numbers {
            let request = &repository[format!("pr{number}")];
            if request.is_null() {
                continue;
            }
            if let Some(error) = &cooldown {
                if Self::next_history_page(request).is_some() {
                    statuses.insert(*number, Err(error.clone()));
                    continue;
                }
            }
            let mut request = request.clone();
            if let Err(error) = self.complete_history(*number, &mut request, &mut pages, &mut nodes, &mut telemetry).await {
                // Classification without a deadline remains an error, but does
                // not invent a timed cooldown; the bounded batch may continue.
                if error.retry_at().is_some() {
                    cooldown = Some(error.clone());
                }
                statuses.insert(*number, Err(error));
                continue;
            }
            request["statusCheckRollup"] = request["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"].clone();
            statuses.insert(
                *number,
                Ok(parse_gh_observation_value_with_crew_identity(
                    &request,
                    observed_at,
                    &self.review_bot_login,
                    self.operator_login.as_deref(),
                    crew_logins.get(number).map(Vec::as_slice).unwrap_or(&[]),
                )),
            );
        }
        Ok(statuses)
    }

    async fn observation_call(
        &self,
        query: &str,
        shape: QueryShape,
        subjects: usize,
        telemetry: &mut ObservationTelemetry<'_>,
    ) -> Result<ParsedObservationResponse, ObservationError> {
        let argument = format!("query={query}");
        let call = telemetry.call(shape, subjects);
        let output = run_output!(self.runner, "gh", &["api", "graphql", "--include", "-f", &argument], execution_root());
        let parsed = output.map(ParsedObservationResponse::from_output);
        call.finish(parsed.as_ref().ok());
        let parsed = parsed?;
        if let Some(limit) = &parsed.limit {
            return Err(ObservationError::RateLimited { budget: "GraphQL".into(), limit: limit.clone() });
        }
        Ok(parsed)
    }

    fn parse_state(state: &str) -> ChangeRequestStatus {
        match state.to_uppercase().as_str() {
            "OPEN" => ChangeRequestStatus::Open,
            "DRAFT" => ChangeRequestStatus::Draft,
            "MERGED" => ChangeRequestStatus::Merged,
            "CLOSED" => ChangeRequestStatus::Closed,
            _ => ChangeRequestStatus::Open,
        }
    }

    fn parse_pull_request(value: &serde_json::Value) -> Option<GhPr> {
        Some(
            GhPr::builder()
                .number(value["number"].as_i64()?)
                .title(value["title"].as_str()?.to_string())
                .head_ref_name(value["head"]["ref"].as_str()?.to_string())
                .maybe_base_ref_name(value["base"]["ref"].as_str().map(str::to_string))
                .state(value["state"].as_str().unwrap_or("open").to_string())
                .maybe_body(value["body"].as_str().map(str::to_string))
                .is_draft(value["draft"].as_bool().unwrap_or(false))
                .maybe_merged_at(value["merged_at"].as_str().map(str::to_string))
                .build(),
        )
    }

    fn gh_pr_to_change_request(&self, pr: &GhPr) -> (String, ChangeRequest) {
        let id = pr.number.to_string();
        let status = if pr.merged_at.is_some() {
            ChangeRequestStatus::Merged
        } else if pr.state.to_uppercase() == "OPEN" && pr.is_draft {
            ChangeRequestStatus::Draft
        } else {
            Self::parse_state(&pr.state)
        };

        (
            id,
            ChangeRequest {
                title: pr.title.clone(),
                branch: pr.head_ref_name.clone(),
                status,
                body: pr.body.clone(),
                provider_name: self.provider_name.clone(),
                provider_display_name: "GitHub".into(),
            },
        )
    }

    fn next_history_page(request: &serde_json::Value) -> Option<HistoryPage> {
        for (field, page) in
            [("comments", HistoryPage::Comments), ("reviews", HistoryPage::Reviews), ("reviewThreads", HistoryPage::Threads)]
        {
            if request[field]["pageInfo"]["hasPreviousPage"] == true {
                return Some(page);
            }
        }
        request["reviewThreads"]["nodes"].as_array()?.iter().enumerate().find_map(|(index, thread)| {
            (thread["comments"]["pageInfo"]["hasPreviousPage"] == true).then_some(HistoryPage::ThreadComments(index))
        })
    }

    async fn complete_history(
        &self,
        number: u64,
        request: &mut serde_json::Value,
        pages: &mut usize,
        nodes: &mut usize,
        telemetry: &mut ObservationTelemetry<'_>,
    ) -> Result<(), ObservationError> {
        while let Some(page) = Self::next_history_page(request) {
            if *pages >= MAX_HISTORY_PAGE_QUERIES || *nodes + page.max_nodes() > MAX_HISTORY_PAGE_NODES {
                return Err(format!("change request {number} review history exceeds per-cycle pagination budget").into());
            }
            let cursor = page.connection(request)["pageInfo"]["startCursor"]
                .as_str()
                .ok_or_else(|| format!("change request {number} truncated review history has no cursor"))?;
            let cursor = serde_json::to_string(cursor).map_err(|error| error.to_string())?;
            let selection = page.selection(request, number, &cursor)?;
            let query = if matches!(page, HistoryPage::ThreadComments(_)) {
                format!("query {{ rateLimit {{ cost }} {selection} }}")
            } else {
                let (owner, name) = self.repo_slug.split_once('/').ok_or("GitHub repository must have owner/name scope")?;
                let owner = serde_json::to_string(owner).map_err(|error| error.to_string())?;
                let name = serde_json::to_string(name).map_err(|error| error.to_string())?;
                format!("query {{ rateLimit {{ cost }} repository(owner:{owner},name:{name}) {{ pr:pullRequest(number:{number}) {{ {selection} }} }} }}")
            };
            let shape = match page {
                HistoryPage::Comments => QueryShape::HistoryComments,
                HistoryPage::Reviews => QueryShape::HistoryReviews,
                HistoryPage::Threads => QueryShape::HistoryThreads,
                HistoryPage::ThreadComments(_) => QueryShape::HistoryThreadComments,
            };
            let output = self.observation_call(&query, shape, 1, telemetry).await?;
            let status = output.response.status;
            let success = output.success;
            let document = output.into_document()?;
            if !success || document["errors"].as_array().is_some_and(|errors| !errors.is_empty()) {
                let messages =
                    document["errors"].as_array().into_iter().flatten().filter_map(|error| error["message"].as_str()).collect::<Vec<_>>();
                return Err(format!("change request {number} review history page failed (HTTP {status}): {messages:?}").into());
            }
            let fetched = match page {
                HistoryPage::ThreadComments(_) => &document["data"]["node"][page.field()],
                _ => &document["data"]["repository"]["pr"][page.field()],
            };
            let older =
                fetched["nodes"].as_array().ok_or_else(|| format!("change request {number} review history page is missing nodes"))?;
            let count = older.len()
                + if matches!(page, HistoryPage::Threads) {
                    older.iter().map(|thread| thread["comments"]["nodes"].as_array().map_or(0, Vec::len)).sum::<usize>()
                } else {
                    0
                };
            let target = page.connection_mut(request);
            let mut combined = older.clone();
            combined.extend(target["nodes"].as_array().into_iter().flatten().cloned());
            target["nodes"] = serde_json::Value::Array(combined);
            target["pageInfo"] = fetched["pageInfo"].clone();
            *pages += 1;
            *nodes += count;
        }
        Ok(())
    }
}

#[async_trait]
impl super::ChangeRequestTracker for GitHubChangeRequest {
    async fn observe_bound(
        &self,
        numbers: &[u64],
        crew_logins: &super::CrewGithubLoginsByRequest,
    ) -> Result<super::BoundObservations, ObservationError> {
        let Some(poll) = &self.poll else {
            return self.observe_changed(numbers, crew_logins).await;
        };
        let mut cache = poll.state.lock().await;
        let key = format!("{}/bound", self.repo_slug);
        let state = cache.entry(key.clone()).or_insert(poll.load(&key)?);
        let mut next = state.clone();
        let mut changed = Vec::new();
        let mut revisions = HashMap::new();
        let mut statuses = HashMap::new();
        for number in numbers {
            let id = number.to_string();
            let endpoint = format!("repos/{}/pulls/{number}", self.repo_slug);
            let response =
                self.api.get_classified_with_headers(&endpoint, execution_root(), &gh_api_channel_label("GET", &endpoint)).await?;
            let value: serde_json::Value = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
            let mut checks = Vec::new();
            if let Some(sha) = value["head"]["sha"].as_str() {
                for suffix in [format!("commits/{sha}/status"), format!("commits/{sha}/check-runs?per_page=100")] {
                    let endpoint = format!("repos/{}/{suffix}", self.repo_slug);
                    let response =
                        self.api.get_classified_with_headers(&endpoint, execution_root(), &gh_api_channel_label("GET", &endpoint)).await?;
                    checks.push((response.etag, response.body));
                }
            }
            let revision =
                serde_json::to_string(&(response.etag, &value["updated_at"], &value["head"]["sha"], checks, crew_logins.get(number)))
                    .map_err(|e| e.to_string())?;
            if next.pulls.revisions.get(&id) == Some(&revision) {
                if let Some(detail) = next.pulls.details.get(&id) {
                    let mut validated: flotilla_resources::ChangeRequestStatus =
                        serde_json::from_value(detail.clone()).map_err(|e| e.to_string())?;
                    let now = Utc::now();
                    validated.title.observed_at = now;
                    validated.author.observed_at = now;
                    validated.review_decision.observed_at = now;
                    validated.review_requested_from_owner.observed_at = now;
                    validated.state.observed_at = now;
                    validated.head_sha.observed_at = now;
                    validated.checks.observed_at = now;
                    validated.review.actionable_at_head.observed_at = now;
                    validated.mergeable.observed_at = now;
                    statuses.insert(*number, Ok(validated));
                    continue;
                }
            }
            revisions.insert(*number, revision);
            changed.push(*number);
        }
        for (number, result) in self.observe_changed(&changed, crew_logins).await? {
            if let Ok(detail) = &result {
                let id = number.to_string();
                next.pulls.revisions.insert(id.clone(), revisions.remove(&number).ok_or("changed PR has no revision")?);
                next.pulls.details.insert(id, serde_json::to_value(detail).map_err(|e| e.to_string())?);
            }
            statuses.insert(number, result);
        }
        poll.save(&key, &next)?;
        *state = next;
        Ok(statuses)
    }

    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        if self.poll.is_some() {
            return Ok(self
                .poll_pull_requests()
                .await?
                .iter()
                .filter(|v| v["state"] == "open")
                .filter_map(|v| Self::parse_pull_request(v).map(|pr| self.gh_pr_to_change_request(&pr)))
                .take(limit)
                .collect());
        }
        let per_page = clamp_per_page(limit);
        let endpoint = format!("repos/{}/pulls?state=open&per_page={}", self.repo_slug, per_page);
        let body = gh_api_get!(self.api, &endpoint, execution_root())?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|e| e.to_string())?;

        Ok(items
            .iter()
            .filter_map(|value| {
                let pr = Self::parse_pull_request(value)?;
                Some(self.gh_pr_to_change_request(&pr))
            })
            .collect())
    }

    async fn find_change_request_by_branch(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, ObservationError> {
        // Bare branches deliberately use the repository owner. Fork heads must
        // be supplied as fork_owner:branch: inferring an unspecified fork owner
        // would require the unfiltered history scan this lookup avoids.
        let head = if branch.contains(':') {
            branch.to_string()
        } else {
            let owner = self.repo_slug.split('/').next().ok_or("repository slug has no owner")?;
            format!("{owner}:{branch}")
        };
        let endpoint = format!("repos/{}/pulls?head={}&state=all&per_page=100", self.repo_slug, urlencoding::encode(&head));
        let response = self.api.get_classified_with_headers(&endpoint, execution_root(), &gh_api_channel_label("GET", &endpoint)).await?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|error| error.to_string())?;
        Ok(items.iter().filter_map(Self::parse_pull_request).next().map(|request| self.gh_pr_to_change_request(&request)))
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        let endpoint = format!("repos/{}/pulls/{}", self.repo_slug, id);
        let body = gh_api_get!(self.api, &endpoint, execution_root())?;
        let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;

        let pr = Self::parse_pull_request(&v).ok_or("malformed pull request")?;
        Ok(self.gh_pr_to_change_request(&pr))
    }

    async fn get_change_request_for_admission(&self, id: &str) -> Result<super::ChangeRequestAdmission, ObservationError> {
        let endpoint = format!("repos/{}/pulls/{}", self.repo_slug, id);
        let response = self.api.get_classified_with_headers(&endpoint, execution_root(), &gh_api_channel_label("GET", &endpoint)).await?;
        let body = response.body;
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        let pull_request = Self::parse_pull_request(&value).ok_or("malformed pull request")?;
        let (id, change_request) = self.gh_pr_to_change_request(&pull_request);
        Ok(super::ChangeRequestAdmission { id, change_request, base_ref: pull_request.base_ref_name })
    }

    async fn update_body(&self, id: &str, body: &str) -> Result<(), String> {
        run!(self.runner, "gh", &["pr", "edit", id, "--repo", &self.repo_slug, "--body", body], execution_root())?;
        Ok(())
    }

    async fn open_in_browser(&self, id: &str) -> Result<(), String> {
        run!(self.runner, "gh", &["pr", "view", id, "--repo", &self.repo_slug, "--web"], execution_root())?;
        Ok(())
    }

    async fn close_change_request(&self, id: &str) -> Result<(), String> {
        run!(self.runner, "gh", &["pr", "close", id, "--repo", &self.repo_slug], execution_root())?;
        Ok(())
    }

    async fn merge_change_request(&self, id: &str) -> Result<(), String> {
        run!(self.runner, "gh", &["pr", "merge", id, "--repo", &self.repo_slug, "--squash"], execution_root())?;
        Ok(())
    }

    async fn list_merged_branch_names(&self, limit: usize) -> Result<Vec<String>, String> {
        if self.poll.is_some() {
            return Ok(self
                .poll_pull_requests()
                .await?
                .iter()
                .filter(|v| v["merged_at"].as_str().is_some())
                .filter_map(|v| v["head"]["ref"].as_str().map(str::to_string))
                .take(limit)
                .collect());
        }
        let per_page = clamp_per_page(limit);
        let endpoint = format!("repos/{}/pulls?state=closed&sort=updated&direction=desc&per_page={}", self.repo_slug, per_page);
        let body = gh_api_get!(self.api, &endpoint, execution_root())?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|e| e.to_string())?;

        Ok(items
            .iter()
            .filter(|v| v["merged_at"].as_str().is_some())
            .filter_map(|v| v["head"]["ref"].as_str().map(|s| s.to_string()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::providers::change_request::ChangeRequestTracker;
    use crate::providers::forge::github::GhApiClient;
    use crate::providers::forge::github::GithubRetrySource;
    use crate::providers::CommandOutput;
    use crate::testkits::replay;
    use crate::testkits::replay::testing::fixture_path;
    use crate::testkits::replay::testing::MockRunner;

    fn branch_lookup_page(items: serde_json::Value, has_next: bool) -> String {
        let link = if has_next { "Link: <https://api.github.com/repos/team/one/pulls?page=2>; rel=\"next\"\r\n" } else { "" };
        format!("HTTP/2 200 OK\r\n{link}\r\n{items}")
    }

    // GitHub's server-side head filter proves both presence and absence in one
    // call; explicit fork owners and URL metacharacters must survive encoding.
    #[tokio::test]
    async fn branch_lookup_filters_head_in_one_classified_request() {
        for (branch, encoded_head) in [("feature/wanted", "team%3Afeature%2Fwanted"), ("fork:feature/a+b", "fork%3Afeature%2Fa%2Bb")] {
            for found in [false, true] {
                // Subprocess boundary: gh emits an HTTP response with headers.
                let items = if found {
                    serde_json::json!([{
                        "number": 7, "title": "Wanted", "head": {"ref": branch.split_once(':').map_or(branch, |(_, name)| name)},
                        "state": "closed", "merged_at": "2026-09-01T00:00:00Z"
                    }])
                } else {
                    serde_json::json!([])
                };
                let runner = Arc::new(MockRunner::new(vec![Ok(branch_lookup_page(items, false))]));
                let provider = GitHubChangeRequest::new(
                    "github".into(),
                    "team/one".into(),
                    Arc::new(GhApiClient::new(runner.clone())),
                    runner.clone(),
                );
                let result = provider.find_change_request_by_branch(branch).await.expect("lookup");
                assert_eq!(result.is_some(), found);
                if let Some((id, request)) = result {
                    assert_eq!(id, "7");
                    assert_eq!(request.branch, branch.split_once(':').map_or(branch, |(_, name)| name));
                    assert_eq!(request.status, ChangeRequestStatus::Merged);
                }
                let calls = runner.calls();
                assert_eq!(calls.len(), 1);
                let expected = format!("repos/team/one/pulls?head={encoded_head}&state=all&per_page=100");
                assert!(calls[0].1.contains(&expected));
            }
        }
    }

    // Glue: the branch lookup must retain the shared REST rate-limit classifier.
    #[tokio::test]
    async fn branch_lookup_preserves_classified_rate_limit() {
        // Subprocess boundary: gh retains HTTP headers on stdout after failure.
        let runner = Arc::new(MockRunner::with_outputs(vec![Ok(CommandOutput {
            stdout: "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1893456000\r\n\r\n{\"message\":\"API rate limit exceeded\"}".into(),
            stderr: "gh: HTTP 403".into(),
            exit_code: Some(1),
        })]));
        let provider =
            GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone());
        let error = provider.find_change_request_by_branch("feature/wanted").await.expect_err("classified rate limit");
        assert!(matches!(error, ObservationError::RateLimited { .. }));
        assert!(error.retry_at().is_some());
        assert_eq!(runner.remaining(), 0);
        assert_eq!(runner.calls().len(), 1);
    }

    // This real API recording covers a merged PR and a nonexistent head using
    // the exact filtered endpoint exercised in production.
    #[tokio::test]
    async fn replayed_real_github_head_lookup_found_and_absent() {
        let fixture = fixture_path("change_request", "github_head_lookup.yaml");
        let session = replay::test_session(&fixture, replay::Masks::new());
        let runner = replay::test_runner(&session);
        let api = replay::test_gh_api(&session);
        let provider = GitHubChangeRequest::new("github".into(), "flotilla-org/flotilla".into(), api, runner);
        let (id, request) = provider.find_change_request_by_branch("fix/observer-budget").await.expect("lookup").expect("merged PR");
        assert_eq!(id, "2506");
        assert_eq!(request.status, ChangeRequestStatus::Merged);
        assert!(provider.find_change_request_by_branch("flotilla-head-lookup-2583-absent").await.expect("absence").is_none());
        session.finish();
    }

    #[tokio::test]
    async fn bot_comment_after_head_is_actionable() {
        let request = serde_json::json!({
            "title": "Keep metadata current", "state": "OPEN", "isDraft": false, "headRefOid": "head-a", "reviewDecision": null,
            "mergeable": "MERGEABLE", "author": {"login": "flotilla-crew"},
            "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "owner"}}]},
            "commits": {"nodes": [{"commit": {
                "committedDate": "2026-09-30T10:00:00Z", "statusCheckRollup": {"contexts": {"nodes": []}}
            }}]},
            "comments": {"nodes": [{
                "databaseId": 42, "createdAt": "2026-09-30T11:00:00Z",
                "author": {"login": "claude"}, "body": "Please fix the race"
            }]},
            "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}
        });
        let response = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
        let runner = Arc::new(MockRunner::new(vec![Ok(response)]));
        let provider =
            GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone())
                .with_operator_login("owner".into());
        let statuses = provider.observe_bound(&[1], &Default::default()).await.expect("observe PR");
        assert_eq!(statuses[&1].as_ref().expect("status").review.actionable_at_head.value, Some(true));
        assert_eq!(statuses[&1].as_ref().expect("status").title.value.as_deref(), Some("Keep metadata current"));
        assert_eq!(statuses[&1].as_ref().expect("status").review_requested_from_owner.value, Some(true));
        assert_eq!(runner.calls().len(), 1);
    }

    #[tokio::test]
    async fn truncated_review_requests_keep_other_bound_observations() {
        let request = serde_json::json!({
            "title": "Keep metadata current", "state": "OPEN", "reviewDecision": null,
            "reviewRequests": {"pageInfo": {"hasNextPage": true}, "nodes": []},
        });
        let response = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
        let runner = Arc::new(MockRunner::new(vec![Ok(response)]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner)
            .with_operator_login("owner".into());
        let statuses = provider.observe_bound(&[1], &Default::default()).await.expect("observe PR");
        let status = statuses[&1].as_ref().expect("preserve PR status");
        assert_eq!(status.title.value.as_deref(), Some("Keep metadata current"));
        assert_eq!(status.state.value, Some(flotilla_resources::ObservedChangeRequestState::Open));
        assert_eq!(status.review_requested_from_owner.value, None);
    }

    #[tokio::test]
    async fn crew_marker_reply_addresses_reviewer_comment() {
        let request = serde_json::json!({
            "state": "OPEN", "headRefOid": "head-a", "reviewDecision": null,
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z", "statusCheckRollup": {"contexts": {"nodes": []}}}}]},
            "comments": {"nodes": [
                {"databaseId": 42, "createdAt": "2026-09-30T11:00:00Z", "author": {"login": "reviewer"}, "body": "Please fix the race"},
                {"databaseId": 43, "createdAt": "2026-09-30T12:00:00Z", "author": {"login": "flotilla-crew"},
                 "body": "Fixed.\n<!-- pr-shepherd-addresses:42 -->"}
            ]}
        });
        let response = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
        let runner = Arc::new(MockRunner::new(vec![Ok(response)]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let statuses = provider.observe_bound(&[1], &BTreeMap::from([(1, vec!["flotilla-crew".to_string()])])).await.expect("observe PR");
        assert_eq!(statuses[&1].as_ref().expect("status").review.actionable_at_head.value, Some(false));
    }

    #[tokio::test]
    async fn bound_requests_only_trust_their_own_crew_identity() {
        let request = serde_json::json!({
            "state": "OPEN", "headRefOid": "head-a", "reviewDecision": null,
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z", "statusCheckRollup": {"contexts": {"nodes": []}}}}]},
            "reviews": {"nodes": [{"fullDatabaseId": "42", "submittedAt": "2026-09-30T11:00:00Z",
                "author": {"login": "reviewer"}, "state": "COMMENTED", "body": "Please fix"}]},
            "comments": {"nodes": [{"databaseId": 43, "createdAt": "2026-09-30T12:00:00Z",
                "author": {"login": "crew-one"}, "body": "<!-- pr-shepherd-addresses:42 -->"}]}
        });
        let response = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request, "pr2": request}}}));
        let runner = Arc::new(MockRunner::new(vec![Ok(response)]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let identities = BTreeMap::from([(1, vec!["crew-one".to_string()]), (2, vec!["crew-two".to_string()])]);
        let statuses = provider.observe_bound(&[1, 2], &identities).await.expect("observe both PRs");
        assert_eq!(statuses[&1].as_ref().expect("first status").review.actionable_at_head.value, Some(false));
        assert_eq!(statuses[&2].as_ref().expect("second status").review.actionable_at_head.value, Some(true));
    }

    #[tokio::test]
    async fn inline_review_comment_after_head_is_actionable() {
        let request = serde_json::json!({
            "state": "OPEN", "headRefOid": "head-a", "reviewDecision": null,
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z", "statusCheckRollup": {"contexts": {"nodes": []}}}}]},
            "comments": {"nodes": []},
            "reviewThreads": {"nodes": [{"comments": {"nodes": [{
                "fullDatabaseId": "51", "createdAt": "2026-09-30T11:00:00Z", "author": {"login": "reviewer"},
                "body": "This branch can panic"
            }]}}]}
        });
        let response = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
        let runner = Arc::new(MockRunner::new(vec![Ok(response)]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let statuses = provider.observe_bound(&[1], &Default::default()).await.expect("observe PR");
        assert_eq!(statuses[&1].as_ref().expect("status").review.actionable_at_head.value, Some(true));
    }

    #[tokio::test]
    async fn pushing_a_new_head_recomputes_review_actionability() {
        let response = |head: &str, committed_at: &str| {
            let request = serde_json::json!({
                "state": "OPEN", "headRefOid": head, "reviewDecision": null,
                "commits": {"nodes": [{"commit": {"committedDate": committed_at, "statusCheckRollup": {"contexts": {"nodes": []}}}}]},
                "comments": {"nodes": [{
                    "databaseId": 42, "createdAt": "2026-09-30T11:00:00Z", "author": {"login": "reviewer"}, "body": "Please fix"
                }]}
            });
            format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}))
        };
        let runner =
            Arc::new(MockRunner::new(vec![Ok(response("head-a", "2026-09-30T10:00:00Z")), Ok(response("head-b", "2026-09-30T12:00:00Z"))]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let first = provider.observe_bound(&[1], &Default::default()).await.expect("first head");
        let second = provider.observe_bound(&[1], &Default::default()).await.expect("new head");
        assert_eq!(first[&1].as_ref().expect("first status").review.actionable_at_head.value, Some(true));
        assert_eq!(second[&1].as_ref().expect("second status").review.actionable_at_head.value, Some(false));
    }

    #[tokio::test]
    async fn delayed_push_uses_github_acceptance_time_for_feedback_cutoff() {
        let request = serde_json::json!({
            "state": "OPEN", "headRefOid": "head-b", "reviewDecision": null,
            "commits": {"nodes": [{"commit": {
                "committedDate": "2026-09-30T10:00:00Z", "pushedDate": "2026-09-30T12:00:00Z",
                "statusCheckRollup": {"contexts": {"nodes": []}}
            }}]},
            "comments": {"nodes": [{
                "databaseId": 42, "createdAt": "2026-09-30T11:00:00Z",
                "author": {"login": "reviewer"}, "body": "Please fix"
            }]},
            "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}
        });
        let response = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
        let runner = Arc::new(MockRunner::new(vec![Ok(response)]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let statuses = provider.observe_bound(&[1], &Default::default()).await.expect("observe PR");
        assert_eq!(statuses[&1].as_ref().expect("status").review.actionable_at_head.value, Some(false));
    }

    #[tokio::test]
    async fn matching_force_push_event_sets_feedback_cutoff_when_pushed_date_is_missing() {
        let request = serde_json::json!({
            "state": "OPEN", "headRefOid": "head-b", "reviewDecision": null,
            "timelineItems": {"nodes": [{"createdAt": "2026-09-30T12:00:00Z", "afterCommit": {"oid": "head-b"}}]},
            "commits": {"nodes": [{"commit": {
                "committedDate": "2026-09-30T10:00:00Z", "pushedDate": null,
                "statusCheckRollup": {"contexts": {"nodes": []}}
            }}]},
            "comments": {"nodes": [{
                "databaseId": 42, "createdAt": "2026-09-30T11:00:00Z",
                "author": {"login": "reviewer"}, "body": "Please fix"
            }]},
            "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}
        });
        let response = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
        let runner = Arc::new(MockRunner::new(vec![Ok(response)]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let statuses = provider.observe_bound(&[1], &Default::default()).await.expect("observe PR");
        assert_eq!(statuses[&1].as_ref().expect("status").review.actionable_at_head.value, Some(false));
    }

    #[tokio::test]
    async fn force_push_without_after_commit_does_not_match_missing_head() {
        let request = serde_json::json!({
            "state": "OPEN", "reviewDecision": null,
            "timelineItems": {"nodes": [{"createdAt": "2026-09-30T12:00:00Z", "afterCommit": null}]},
            "commits": {"nodes": [{"commit": {
                "pushedDate": "2026-09-30T10:00:00Z", "statusCheckRollup": {"contexts": {"nodes": []}}
            }}]},
            "comments": {"nodes": [{"databaseId": 42, "createdAt": "2026-09-30T11:00:00Z",
                "author": {"login": "reviewer"}, "body": "Please fix"}]},
            "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}
        });
        let response = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
        let runner = Arc::new(MockRunner::new(vec![Ok(response)]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let statuses = provider.observe_bound(&[1], &Default::default()).await.expect("observe PR");
        assert_eq!(statuses[&1].as_ref().expect("status").review.actionable_at_head.value, Some(true));
    }

    #[tokio::test]
    async fn review_with_feedback_is_actionable_until_crew_marks_it_addressed() {
        let response = |addressed: bool| {
            let comments = if addressed {
                vec![serde_json::json!({
                    "databaseId": 88, "createdAt": "2026-09-30T12:00:00Z", "author": {"login": "flotilla-crew"},
                    "body": "Fixed.\n<!-- pr-shepherd-addresses:77 -->"
                })]
            } else {
                Vec::new()
            };
            let request = serde_json::json!({
                "state": "OPEN", "headRefOid": "head-a", "reviewDecision": null,
                "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z", "statusCheckRollup": {"contexts": {"nodes": []}}}}]},
                "comments": {"nodes": comments},
                "reviews": {"nodes": [{
                    "fullDatabaseId": "77", "submittedAt": "2026-09-30T11:00:00Z",
                    "author": {"login": "reviewer"}, "body": "Please fix the race", "state": "COMMENTED"
                }]}
            });
            format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}))
        };
        let runner = Arc::new(MockRunner::new(vec![Ok(response(false)), Ok(response(true))]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let first =
            provider.observe_bound(&[1], &BTreeMap::from([(1, vec!["flotilla-crew".to_string()])])).await.expect("unaddressed review");
        let second =
            provider.observe_bound(&[1], &BTreeMap::from([(1, vec!["flotilla-crew".to_string()])])).await.expect("addressed review");
        assert_eq!(first[&1].as_ref().expect("first status").review.actionable_at_head.value, Some(true));
        assert_eq!(second[&1].as_ref().expect("second status").review.actionable_at_head.value, Some(false));
    }

    #[tokio::test]
    async fn formal_changes_requested_review_is_actionable_until_addressed() {
        let response = |addressed: bool| {
            let comments = if addressed {
                vec![serde_json::json!({
                    "databaseId": 88, "createdAt": "2026-09-30T12:00:00Z", "author": {"login": "flotilla-crew"},
                    "body": "Fixed.\n<!-- pr-shepherd-addresses:77 -->"
                })]
            } else {
                Vec::new()
            };
            let request = serde_json::json!({
                "state": "OPEN", "headRefOid": "head-a", "reviewDecision": null,
                "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z", "statusCheckRollup": {"contexts": {"nodes": []}}}}]},
                "comments": {"nodes": comments},
                "reviews": {"nodes": [{
                    "fullDatabaseId": "77", "submittedAt": "2026-09-30T09:00:00Z",
                    "author": {"login": "reviewer"}, "state": "CHANGES_REQUESTED"
                }]}
            });
            format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}))
        };
        let runner = Arc::new(MockRunner::new(vec![Ok(response(false)), Ok(response(true))]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let first = provider
            .observe_bound(&[1], &BTreeMap::from([(1, vec!["flotilla-crew".to_string()])]))
            .await
            .expect("unaddressed formal review");
        let second =
            provider.observe_bound(&[1], &BTreeMap::from([(1, vec!["flotilla-crew".to_string()])])).await.expect("addressed formal review");
        assert_eq!(first[&1].as_ref().expect("first status").review.actionable_at_head.value, Some(true));
        assert_eq!(second[&1].as_ref().expect("second status").review.actionable_at_head.value, Some(false));
    }

    #[tokio::test]
    async fn truncated_review_history_is_not_reported_as_clear() {
        let request = serde_json::json!({
            "state": "OPEN", "headRefOid": "head-a", "reviewDecision": null,
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z", "statusCheckRollup": {"contexts": {"nodes": []}}}}]},
            "comments": {"pageInfo": {"hasPreviousPage": true}, "nodes": []},
            "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}
        });
        let response = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
        let runner = Arc::new(MockRunner::new(vec![Ok(response)]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let statuses = provider.observe_bound(&[1], &Default::default()).await.expect("observe PR");
        assert!(statuses[&1].as_ref().expect_err("truncated history").to_string().contains("truncated"));
    }

    #[tokio::test]
    async fn older_comment_page_can_make_busy_request_actionable() {
        let request = serde_json::json!({
            "state": "OPEN", "headRefOid": "head-a", "reviewDecision": null,
            "commits": {"nodes": [{"commit": {"pushedDate": "2026-09-30T10:00:00Z", "statusCheckRollup": {"contexts": {"nodes": []}}}}]},
            "comments": {"pageInfo": {"hasPreviousPage": true, "startCursor": "newer"}, "nodes": []},
            "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}
        });
        let first = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
        let older = serde_json::json!({"data": {"repository": {"pr": {"comments": {
            "pageInfo": {"hasPreviousPage": false, "startCursor": "oldest"},
            "nodes": [{"databaseId": 42, "createdAt": "2026-09-30T11:00:00Z", "author": {"login": "reviewer"}, "body": "Please fix"}]
        }}}}});
        let runner = Arc::new(MockRunner::new(vec![Ok(first), Ok(format!("HTTP/2 200 OK\r\n\r\n{older}"))]));
        let provider =
            GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone());
        let statuses = provider.observe_bound(&[1], &Default::default()).await.expect("observe PR");
        assert_eq!(statuses[&1].as_ref().expect("complete history").review.actionable_at_head.value, Some(true));
        assert_eq!(runner.calls().len(), 2);
    }

    #[tokio::test]
    async fn older_review_thread_and_thread_comment_pages_are_observed() {
        for case in ["reviews", "reviewThreads", "threadComments"] {
            let mut request = serde_json::json!({
                "state": "OPEN", "headRefOid": "head-a", "reviewDecision": null,
                "commits": {"nodes": [{"commit": {"pushedDate": "2026-09-30T10:00:00Z", "statusCheckRollup": {"contexts": {"nodes": []}}}}]},
                "comments": {"nodes": []}, "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}
            });
            let comment = serde_json::json!({
                "fullDatabaseId": "42", "createdAt": "2026-09-30T11:00:00Z",
                "author": {"login": "reviewer"}, "body": "Please fix"
            });
            let page = serde_json::json!({"pageInfo": {"hasPreviousPage": false, "startCursor": "older"}});
            let older = match case {
                "reviews" => {
                    request["reviews"]["pageInfo"] = serde_json::json!({"hasPreviousPage": true, "startCursor": "newer"});
                    serde_json::json!({"data": {"repository": {"pr": {"reviews": {
                        "pageInfo": page["pageInfo"], "nodes": [{"fullDatabaseId": "42", "submittedAt": "2026-09-30T11:00:00Z",
                        "author": {"login": "reviewer"}, "body": "Please fix", "state": "COMMENTED"}]
                    }}}}})
                }
                "reviewThreads" => {
                    request["reviewThreads"]["pageInfo"] = serde_json::json!({"hasPreviousPage": true, "startCursor": "newer"});
                    serde_json::json!({"data": {"repository": {"pr": {"reviewThreads": {
                        "pageInfo": page["pageInfo"], "nodes": [{"id": "thread-a", "isResolved": false,
                        "comments": {"pageInfo": {"hasPreviousPage": false}, "nodes": [comment]}}]
                    }}}}})
                }
                _ => {
                    request["reviewThreads"]["nodes"] = serde_json::json!([{"id": "thread-a", "isResolved": false,
                        "comments": {"pageInfo": {"hasPreviousPage": true, "startCursor": "newer"}, "nodes": []}}]);
                    serde_json::json!({"data": {"node": {"comments": {"pageInfo": page["pageInfo"], "nodes": [comment]}}}})
                }
            };
            let first = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
            let runner = Arc::new(MockRunner::new(vec![Ok(first), Ok(format!("HTTP/2 200 OK\r\n\r\n{older}"))]));
            let provider =
                GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone());
            let statuses = provider.observe_bound(&[1], &Default::default()).await.expect("observe PR");
            assert_eq!(statuses[&1].as_ref().expect(case).review.actionable_at_head.value, Some(true), "{case}");
            assert_eq!(runner.calls().len(), 2, "{case}");
        }
    }

    // #2499: a rate-limited history call ends network pagination for the
    // whole batch, preserving requests already complete in the initial response.
    #[hegel::test]
    fn history_limit_stops_followups_for_every_remaining_subject(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Empty through multi-subject batches, including the fleet's six-subject
        // flotilla volume. All busy subjects have an unfinished history page.
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(10));
        let requests = (1..=count)
            .map(|number| {
                (
                    format!("pr{number}"),
                    serde_json::json!({
                        "state": "OPEN", "comments": {"pageInfo": {"hasPreviousPage": true, "startCursor": "more"}, "nodes": []}
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let responses = if count == 0 {
            Vec::new()
        } else {
            vec![
            Ok(format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": requests}}))),
            Ok("HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 4989\r\nX-RateLimit-Reset: 1893456000\r\nRetry-After: 30\r\n\r\n{\"message\":\"secondary rate limit\"}".into()),
        ]
        };
        // MockRunner stands in for the subprocess/GitHub boundary; unexpected
        // extra requests fail rather than silently granting another response.
        let runner = Arc::new(MockRunner::new(responses));
        let provider =
            GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let numbers = (1..=count as u64).collect::<Vec<_>>();
        let statuses = runtime.block_on(provider.observe_bound(&numbers, &Default::default())).expect("batch results");
        assert_eq!(statuses.len(), count);
        assert!(statuses.values().all(|status| status.as_ref().is_err_and(|error| error.to_string().contains("kind=secondary"))));
        assert_eq!(runner.calls().len(), if count == 0 { 0 } else { 2 }, "no further history calls during a cooldown");
    }

    #[tokio::test]
    async fn pagination_budget_reports_busy_request_without_hiding_healthy_request() {
        let busy = serde_json::json!({
            "state": "OPEN", "headRefOid": "busy", "reviewDecision": null,
            "comments": {"pageInfo": {"hasPreviousPage": true, "startCursor": "more"}, "nodes": []},
            "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}
        });
        let healthy = serde_json::json!({"title": "Healthy", "state": "OPEN", "reviewDecision": null,
            "comments": {"nodes": []}, "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}});
        let first = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": busy, "pr2": healthy}}}));
        let page = format!(
            "HTTP/2 200 OK\r\n\r\n{}",
            serde_json::json!({"data": {"repository": {"pr": {"comments": {
                "pageInfo": {"hasPreviousPage": true, "startCursor": "more"}, "nodes": []
            }}}}})
        );
        let responses = std::iter::once(Ok(first)).chain(std::iter::repeat_n(Ok(page), MAX_HISTORY_PAGE_QUERIES)).collect();
        let runner = Arc::new(MockRunner::new(responses));
        let provider =
            GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone());
        let statuses = provider.observe_bound(&[1, 2], &Default::default()).await.expect("observe both PRs");
        assert!(statuses[&1].as_ref().expect_err("budget exhausted").to_string().contains("pagination budget"));
        assert_eq!(statuses[&2].as_ref().expect("healthy PR").title.value.as_deref(), Some("Healthy"));
        assert_eq!(runner.calls().len(), MAX_HISTORY_PAGE_QUERIES + 1);
    }

    #[tokio::test]
    async fn caller_history_priority_preserves_shared_budget_and_errors() {
        let busy = serde_json::json!({"state": "OPEN", "reviewDecision": null,
            "comments": {"pageInfo": {"hasPreviousPage": true, "startCursor": "more"}, "nodes": []},
            "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}});
        let initial = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": busy, "pr2": busy}}}));
        let page = format!(
            "HTTP/2 200 OK\r\n\r\n{}",
            serde_json::json!({"data": {"repository": {"pr": {"comments": {
                "pageInfo": {"hasPreviousPage": true, "startCursor": "more"}, "nodes": []}}}}})
        );
        let cycle = std::iter::once(Ok(initial)).chain(std::iter::repeat_n(Ok(page), MAX_HISTORY_PAGE_QUERIES)).collect::<Vec<_>>();
        let runner = Arc::new(MockRunner::new(cycle.iter().cloned().chain(cycle.iter().cloned()).collect()));
        let provider =
            GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone());
        for priority in [[1, 2], [2, 1]] {
            let statuses = provider.observe_bound(&priority, &Default::default()).await.expect("observe cycle");
            assert!(statuses.values().all(Result::is_err), "incomplete histories remain errors");
        }
        let calls = runner.calls();
        assert_eq!(calls.len(), 2 * (MAX_HISTORY_PAGE_QUERIES + 1));
        assert!(calls[1].1.last().expect("query").contains("number:1"));
        assert!(
            calls[MAX_HISTORY_PAGE_QUERIES + 2].1.last().expect("query").contains("number:2"),
            "later request must receive the next cycle's pagination budget"
        );
    }

    #[tokio::test]
    async fn pagination_node_budget_stops_large_thread_pages_before_query_limit() {
        let request = serde_json::json!({"state": "OPEN", "reviewDecision": null,
            "reviewThreads": {"pageInfo": {"hasPreviousPage": true, "startCursor": "more"}, "nodes": []}});
        let first = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": request}}}));
        let comments = vec![serde_json::json!({"body": "note"}); THREAD_COMMENT_PREVIEW_SIZE];
        let threads = vec![
            serde_json::json!({"isResolved": true,
            "comments": {"pageInfo": {"hasPreviousPage": false}, "nodes": comments}});
            THREAD_PAGE_SIZE
        ];
        let page = format!(
            "HTTP/2 200 OK\r\n\r\n{}",
            serde_json::json!({"data": {"repository": {"pr": {"reviewThreads": {
                "pageInfo": {"hasPreviousPage": true, "startCursor": "more"}, "nodes": threads
            }}}}})
        );
        let responses = std::iter::once(Ok(first)).chain(std::iter::repeat_n(Ok(page), 3)).collect();
        let runner = Arc::new(MockRunner::new(responses));
        let provider =
            GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone());
        let statuses = provider.observe_bound(&[1], &Default::default()).await.expect("observe PR");
        assert!(statuses[&1].as_ref().expect_err("node budget exhausted").to_string().contains("pagination budget"));
        assert_eq!(runner.calls().len(), 4, "three 220-node pages leave too little budget for a fourth");
    }

    #[tokio::test]
    async fn failed_history_page_is_local_to_its_request() {
        let busy = serde_json::json!({"state": "OPEN", "reviewDecision": null,
            "comments": {"pageInfo": {"hasPreviousPage": true, "startCursor": "more"}, "nodes": []}});
        let healthy = serde_json::json!({"title": "Healthy", "state": "OPEN", "reviewDecision": null});
        let first = format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": {"pr1": busy, "pr2": healthy}}}));
        let failed = "HTTP/2 200 OK\r\n\r\n{\"errors\":[{\"message\":\"cursor expired\"}]}".to_string();
        let runner = Arc::new(MockRunner::new(vec![Ok(first), Ok(failed)]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let statuses = provider.observe_bound(&[1, 2], &Default::default()).await.expect("observe both PRs");
        assert!(statuses[&1].as_ref().expect_err("failed page").to_string().contains("cursor expired"));
        assert_eq!(statuses[&2].as_ref().expect("healthy PR").title.value.as_deref(), Some("Healthy"));
    }

    #[tokio::test]
    async fn replayed_real_batched_github_observation() {
        let fixture = fixture_path("change_request", "github_observer_busy.yaml");
        let session = replay::test_session(&fixture, replay::Masks::new());
        let runner = replay::test_runner(&session);
        let provider =
            GitHubChangeRequest::new("github".into(), "flotilla-org/flotilla".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let statuses = provider.observe_bound(&[2320], &Default::default()).await.expect("observe real PR");
        let status = statuses[&2320].as_ref().expect("complete real PR observation");
        assert_eq!(status.title.value.as_deref(), Some("fix: wake crews for unaddressed PR review feedback"));
        assert_eq!(status.head_sha.value.as_deref(), Some("1256f96194276ce7b7c7d4fa614d6782dbf1fabf"));
        assert_eq!(status.review.actionable_at_head.value, Some(true));
        session.finish();
    }

    #[tokio::test]
    async fn bound_observation_batches_n_requests_across_two_repositories() {
        fn response(numbers: &[u64]) -> String {
            let requests = numbers
                .iter()
                .map(|number| {
                    (
                        format!("pr{number}"),
                        serde_json::json!({
                            "state": "OPEN", "isDraft": false, "headRefOid": "abc", "reviewDecision": null,
                            "mergeable": "MERGEABLE", "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": []}}}}]}
                        }),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            format!("HTTP/2 200 OK\r\n\r\n{}", serde_json::json!({"data": {"repository": requests}}))
        }

        let runner = Arc::new(MockRunner::new(vec![Ok(response(&[1, 2, 3, 4])), Ok(response(&[5, 6, 7]))]));
        let api = Arc::new(GhApiClient::new(runner.clone()));
        let first = GitHubChangeRequest::new("github".into(), "team/one".into(), api.clone(), runner.clone());
        let second = GitHubChangeRequest::new("github".into(), "team/two".into(), api, runner.clone());

        assert_eq!(first.observe_bound(&[1, 2, 3, 4], &Default::default()).await.expect("first repository").len(), 4);
        assert_eq!(second.observe_bound(&[5, 6, 7], &Default::default()).await.expect("second repository").len(), 3);
        let calls = runner.calls();
        assert_eq!(calls.len(), 2, "one request per repository, independent of bound CR count");
        assert!(calls[0].1.iter().any(|argument| argument.contains("pr1:") && argument.contains("pr4:")));
        assert!(calls[1].1.iter().any(|argument| argument.contains("pr5:") && argument.contains("pr7:")));
    }

    #[tokio::test]
    async fn graphql_rate_limit_reports_budget_identity_and_reset() {
        let runner = Arc::new(MockRunner::new(vec![Ok(
            "HTTP/2 200 OK\r\nX-RateLimit-Reset: 1784736000\r\nX-RateLimit-Remaining: 0\r\n\r\n{\"errors\":[{\"message\":\"API rate limit exceeded\"}]}".into(),
        )]));
        let api = Arc::new(GhApiClient::new(runner.clone()));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), api, runner);
        let error = provider.observe_bound(&[1], &Default::default()).await.expect_err("rate limited");
        assert!(
            error.to_string().contains(
                "rate limited (budget=GraphQL, identity=host gh login, kind=primary, retry_source=x-ratelimit-reset, retry_at=2026-"
            ),
            "{error}"
        );
        assert!(error.retry_at().is_some());
    }

    // #2510: missing headers do not discard proven primary classification or
    // fabricate a retry deadline. Glue: classifier matrix owns response variants.
    #[tokio::test]
    async fn primary_limit_without_deadline_keeps_classification_without_timed_wait() {
        use crate::providers::forge::github::GithubRateLimitKind;
        let runner = Arc::new(MockRunner::new(vec![Ok(
            "HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\n\r\n{\"message\":\"API rate limit exceeded\"}".into(),
        )]));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner);
        let error = provider.observe_bound(&[1], &Default::default()).await.expect_err("classified limit");
        assert!(matches!(&error, ObservationError::RateLimited { limit, .. }
            if limit.kind == GithubRateLimitKind::Primary && limit.retry_source == GithubRetrySource::Unavailable));
        assert_eq!(error.retry_at(), None, "no fabricated deadline for a cache, completion or Landing wait");
    }

    #[tokio::test]
    async fn missing_bound_request_does_not_hide_other_observations() {
        let response = serde_json::json!({
            "data": {"repository": {"pr1": null, "pr2": {
                "state": "OPEN", "isDraft": false, "headRefOid": "abc", "reviewDecision": null,
                "mergeable": "MERGEABLE", "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": []}}}}]}
            }}},
            "errors": [{"type": "NOT_FOUND", "path": ["repository", "pr1"], "message": "Could not resolve pull request"}]
        });
        let runner = Arc::new(MockRunner::new(vec![Ok(format!("HTTP/2 200 OK\r\n\r\n{response}"))]));
        let api = Arc::new(GhApiClient::new(runner.clone()));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), api, runner);
        let statuses = provider.observe_bound(&[1, 2], &Default::default()).await.expect("partial GraphQL result");
        assert!(!statuses.contains_key(&1));
        assert!(statuses.contains_key(&2));
    }
    #[tokio::test]
    async fn bound_details_survive_restart_and_refresh_when_checks_change() {
        fn rest(etag: &str, body: serde_json::Value) -> String {
            format!("HTTP/2 200 OK\r\nETag: {etag}\r\n\r\n{body}")
        }
        fn detail(title: &str) -> String {
            format!(
                "HTTP/2 200 OK\r\n\r\n{}",
                serde_json::json!({"data":{"repository":{"pr1":{
                "title":title,"state":"OPEN","headRefOid":"abc","reviewDecision":null,"mergeable":"MERGEABLE",
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}
            }},"rateLimit":{"cost":1}}})
            )
        }
        let unchanged = "HTTP/2 304 Not Modified\r\n\r\n".to_string();
        let runner = Arc::new(MockRunner::new(vec![
            Ok(rest("pr", serde_json::json!({"number":1,"updated_at":"2026-10-07T00:00:00Z","head":{"sha":"abc"}}))),
            Ok(rest("status", serde_json::json!({"state":"pending"}))),
            Ok(rest("checks-1", serde_json::json!({"check_runs":[]}))),
            Ok(detail("Initial")),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(unchanged.clone()),
            Ok(rest("checks-2", serde_json::json!({"check_runs":[{"status":"completed"}]}))),
            Ok(detail("Refreshed")),
        ]));
        let directory = tempfile::tempdir().expect("persistent observation cache");
        let make = || {
            GitHubChangeRequest::new(
                "github".into(),
                "team/repo".into(),
                Arc::new(GhApiClient::new(runner.clone()).with_persistence(directory.path().join("rest"))),
                runner.clone(),
            )
            .with_poll_directory(directory.path().join("bound"))
        };
        let provider = make();
        let first = provider.observe_bound(&[1], &Default::default()).await.expect("first");
        assert_eq!(first[&1].as_ref().expect("observed").title.value.as_deref(), Some("Initial"));
        provider.observe_bound(&[1], &Default::default()).await.expect("quiet");
        drop(provider);
        let provider = make();
        let restarted = provider.observe_bound(&[1], &Default::default()).await.expect("restart");
        assert_eq!(restarted[&1].as_ref().expect("cached").title.value.as_deref(), Some("Initial"));
        let changed = provider.observe_bound(&[1], &Default::default()).await.expect("checks updated");
        assert_eq!(changed[&1].as_ref().expect("refreshed").title.value.as_deref(), Some("Refreshed"));
        let calls = runner.calls();
        assert_eq!(calls.iter().filter(|(_, args)| args.contains(&"graphql".into())).count(), 2);
        assert!(calls[4..10].iter().all(|(_, args)| args.iter().any(|arg| arg.starts_with("If-None-Match:"))));
        assert_eq!(runner.remaining(), 0);
    }
}
