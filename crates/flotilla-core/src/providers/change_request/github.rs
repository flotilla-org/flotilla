use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;

use crate::{
    change_request_observer::{parse_gh_observation_with_crew_identity, DEFAULT_REVIEW_BOT_LOGIN},
    providers::{
        gh_api_get, gh_api_get_with_headers,
        github_api::{clamp_per_page, parse_gh_api_response, rate_limit_error_from_response, GhApi},
        run, run_output,
        types::*,
        CommandRunner,
    },
};

fn execution_root() -> &'static Path {
    Path::new("/")
}

const MAX_BRANCH_LOOKUP_PAGES: usize = 10;

pub struct GitHubChangeRequest {
    provider_name: String,
    repo_slug: String,
    api: Arc<dyn GhApi>,
    runner: Arc<dyn CommandRunner>,
    review_bot_login: String,
    operator_login: Option<String>,
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
        Self { provider_name, repo_slug, api, runner, review_bot_login: DEFAULT_REVIEW_BOT_LOGIN.to_string(), operator_login: None }
    }

    pub fn with_review_bot_login(mut self, login: String) -> Self {
        self.review_bot_login = login;
        self
    }

    pub fn with_operator_login(mut self, login: String) -> Self {
        self.operator_login = Some(login);
        self
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

        (id, ChangeRequest {
            title: pr.title.clone(),
            branch: pr.head_ref_name.clone(),
            status,
            body: pr.body.clone(),
            provider_name: self.provider_name.clone(),
            provider_display_name: "GitHub".into(),
        })
    }
}

#[async_trait]
impl super::ChangeRequestTracker for GitHubChangeRequest {
    async fn observe_bound(&self, numbers: &[u64]) -> Result<super::BoundObservations, String> {
        self.observe_bound_with_crew_identity(numbers, None).await
    }

    async fn observe_bound_with_crew_identity(
        &self,
        numbers: &[u64],
        crew_login: Option<&str>,
    ) -> Result<super::BoundObservations, String> {
        if numbers.is_empty() {
            return Ok(HashMap::new());
        }
        let (owner, name) = self.repo_slug.split_once('/').ok_or("GitHub repository must have owner/name scope")?;
        let owner = serde_json::to_string(owner).map_err(|error| error.to_string())?;
        let name = serde_json::to_string(name).map_err(|error| error.to_string())?;
        let mut query = format!("query {{ repository(owner:{owner}, name:{name}) {{");
        // Keep every bound CR in one repository query so request count is
        // independent of convoy count. If a repository exceeds GitHub's query
        // limits, surface the forge error instead of silently omitting CRs.
        for number in numbers {
            query.push_str(&format!(" pr{number}: pullRequest(number:{number}) {{ title state isDraft headRefOid reviewDecision mergeable author {{ login }} reviewRequests(first:100) {{ pageInfo {{ hasNextPage }} nodes {{ requestedReviewer {{ __typename ... on User {{ login }} }} }} }} comments(last:100) {{ pageInfo {{ hasPreviousPage }} nodes {{ databaseId createdAt author {{ login __typename }} body }} }} reviews(last:100) {{ pageInfo {{ hasPreviousPage }} nodes {{ fullDatabaseId submittedAt author {{ login __typename }} state body }} }} reviewThreads(last:100) {{ pageInfo {{ hasPreviousPage }} nodes {{ isResolved comments(last:10) {{ pageInfo {{ hasPreviousPage }} nodes {{ fullDatabaseId createdAt author {{ login __typename }} body }} }} }} }} commits(last:1) {{ nodes {{ commit {{ committedDate statusCheckRollup {{ contexts(first:100) {{ nodes {{ ... on CheckRun {{ conclusion status }} ... on StatusContext {{ state }} }} }} }} }} }} }} }}"));
        }
        query.push_str(" } }");
        let argument = format!("query={query}");
        let output = run_output!(self.runner, "gh", &["api", "graphql", "--include", "-f", &argument], execution_root())?;
        if let Some(error) = rate_limit_error_from_response(&output.stdout, "GraphQL") {
            return Err(error);
        }
        let response = parse_gh_api_response(&output.stdout);
        let document: serde_json::Value = serde_json::from_str(&response.body)
            .map_err(|error| format!("decode GitHub GraphQL observation: {error}; {}", output.stderr))?;
        if let Some(errors) = document["errors"].as_array() {
            let unexpected = errors.iter().filter(|error| error["type"] != "NOT_FOUND").collect::<Vec<_>>();
            if !unexpected.is_empty() {
                return Err(format!("GitHub GraphQL observation: {unexpected:?}"));
            }
        } else if !output.success {
            return Err(format!("GitHub GraphQL observation failed: {}", output.stderr));
        }
        let repository = &document["data"]["repository"];
        if !repository.is_object() {
            return Err(format!("GitHub GraphQL repository {} unavailable", self.repo_slug));
        }
        let observed_at = Utc::now();
        let mut statuses = HashMap::new();
        for number in numbers {
            let request = &repository[format!("pr{number}")];
            if request.is_null() {
                continue;
            }
            let truncated = ["comments", "reviews", "reviewThreads"]
                .iter()
                .any(|connection| request[connection]["pageInfo"]["hasPreviousPage"] == true)
                || request["reviewThreads"]["nodes"]
                    .as_array()
                    .is_some_and(|threads| threads.iter().any(|thread| thread["comments"]["pageInfo"]["hasPreviousPage"] == true));
            if truncated {
                statuses.insert(*number, Err(format!("change request {number} review history truncated by GitHub pagination")));
                continue;
            }
            let mut request = request.clone();
            request["statusCheckRollup"] = request["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"].clone();
            statuses.insert(
                *number,
                Ok(parse_gh_observation_with_crew_identity(
                    &request.to_string(),
                    observed_at,
                    &self.review_bot_login,
                    self.operator_login.as_deref(),
                    crew_login,
                )?),
            );
        }
        Ok(statuses)
    }

    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
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

    async fn find_change_request_by_branch(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, String> {
        for page in 1..=MAX_BRANCH_LOOKUP_PAGES {
            let endpoint = if page == 1 {
                format!("repos/{}/pulls?state=all&per_page=100", self.repo_slug)
            } else {
                format!("repos/{}/pulls?state=all&per_page=100&page={page}", self.repo_slug)
            };
            let response = gh_api_get_with_headers!(self.api, &endpoint, execution_root())?;
            let items: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|error| error.to_string())?;
            if let Some(pull_request) =
                items.iter().filter_map(Self::parse_pull_request).find(|pull_request| pull_request.head_ref_name == branch)
            {
                return Ok(Some(self.gh_pr_to_change_request(&pull_request)));
            }
            if !response.has_next_page {
                return Ok(None);
            }
        }
        Err(format!("GitHub pull request lookup for branch {branch} exceeded {MAX_BRANCH_LOOKUP_PAGES} pages"))
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        let endpoint = format!("repos/{}/pulls/{}", self.repo_slug, id);
        let body = gh_api_get!(self.api, &endpoint, execution_root())?;
        let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;

        let pr = Self::parse_pull_request(&v).ok_or("malformed pull request")?;
        Ok(self.gh_pr_to_change_request(&pr))
    }

    async fn get_change_request_for_admission(&self, id: &str) -> Result<super::ChangeRequestAdmission, String> {
        let endpoint = format!("repos/{}/pulls/{}", self.repo_slug, id);
        let body = gh_api_get!(self.api, &endpoint, execution_root())?;
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        let pull_request = Self::parse_pull_request(&value).ok_or("malformed pull request")?;
        let (id, change_request) = self.gh_pr_to_change_request(&pull_request);
        Ok(super::ChangeRequestAdmission { id, change_request, base_ref: pull_request.base_ref_name })
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
    use super::*;
    use crate::providers::{change_request::ChangeRequestTracker, github_api::GhApiClient, testing::MockRunner};

    fn branch_lookup_page(items: serde_json::Value, has_next: bool) -> String {
        let link = if has_next { "Link: <https://api.github.com/repos/team/one/pulls?page=2>; rel=\"next\"\r\n" } else { "" };
        format!("HTTP/2 200 OK\r\n{link}\r\n{items}")
    }

    #[tokio::test]
    async fn branch_lookup_finds_merged_request_on_later_page_and_stops() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok(branch_lookup_page(
                serde_json::json!([{
                    "number": 9, "title": "Other", "head": {"ref": "other"}, "state": "closed", "merged_at": null
                }]),
                true,
            )),
            Ok(branch_lookup_page(
                serde_json::json!([{
                    "number": 7, "title": "Wanted", "head": {"ref": "feature/wanted"},
                    "state": "closed", "merged_at": "2026-09-01T00:00:00Z"
                }]),
                true,
            )),
        ]));
        let provider =
            GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone());

        let found = provider.find_change_request_by_branch("feature/wanted").await.expect("lookup").expect("later-page request");
        assert_eq!(found.0, "7");
        assert_eq!(found.1.status, ChangeRequestStatus::Merged);
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1.iter().any(|arg| arg == "repos/team/one/pulls?state=all&per_page=100"));
        assert!(calls[1].1.iter().any(|arg| arg.contains("per_page=100&page=2")));
    }

    #[tokio::test]
    async fn branch_lookup_stops_when_github_has_no_next_page() {
        let runner = Arc::new(MockRunner::new(vec![Ok(branch_lookup_page(
            serde_json::json!([{
                "number": 9, "title": "Other", "head": {"ref": "other"}, "state": "open"
            }]),
            false,
        ))]));
        let provider =
            GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone());

        assert!(provider.find_change_request_by_branch("feature/wanted").await.expect("lookup").is_none());
        assert_eq!(runner.calls().len(), 1);
    }

    #[tokio::test]
    async fn branch_lookup_reports_when_page_budget_cannot_prove_absence() {
        let page = branch_lookup_page(serde_json::json!([]), true);
        let runner = Arc::new(MockRunner::new(vec![Ok(page); MAX_BRANCH_LOOKUP_PAGES]));
        let provider =
            GitHubChangeRequest::new("github".into(), "team/one".into(), Arc::new(GhApiClient::new(runner.clone())), runner.clone());

        let error = provider.find_change_request_by_branch("feature/wanted").await.expect_err("lookup cannot prove absence");
        assert!(error.contains("exceeded 10 pages"), "{error}");
        assert_eq!(runner.calls().len(), MAX_BRANCH_LOOKUP_PAGES);
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
        let statuses = provider.observe_bound(&[1]).await.expect("observe PR");
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
        let statuses = provider.observe_bound(&[1]).await.expect("observe PR");
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
        let statuses = provider.observe_bound_with_crew_identity(&[1], Some("flotilla-crew[bot]")).await.expect("observe PR");
        assert_eq!(statuses[&1].as_ref().expect("status").review.actionable_at_head.value, Some(false));
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
        let statuses = provider.observe_bound(&[1]).await.expect("observe PR");
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
        let first = provider.observe_bound(&[1]).await.expect("first head");
        let second = provider.observe_bound(&[1]).await.expect("new head");
        assert_eq!(first[&1].as_ref().expect("first status").review.actionable_at_head.value, Some(true));
        assert_eq!(second[&1].as_ref().expect("second status").review.actionable_at_head.value, Some(false));
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
        let first = provider.observe_bound_with_crew_identity(&[1], Some("flotilla-crew[bot]")).await.expect("unaddressed review");
        let second = provider.observe_bound_with_crew_identity(&[1], Some("flotilla-crew[bot]")).await.expect("addressed review");
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
        let first = provider.observe_bound_with_crew_identity(&[1], Some("flotilla-crew[bot]")).await.expect("unaddressed formal review");
        let second = provider.observe_bound_with_crew_identity(&[1], Some("flotilla-crew[bot]")).await.expect("addressed formal review");
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
        let statuses = provider.observe_bound(&[1]).await.expect("observe PR");
        assert!(statuses[&1].as_ref().expect_err("truncated history").contains("truncated"));
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

        assert_eq!(first.observe_bound(&[1, 2, 3, 4]).await.expect("first repository").len(), 4);
        assert_eq!(second.observe_bound(&[5, 6, 7]).await.expect("second repository").len(), 3);
        let calls = runner.calls();
        assert_eq!(calls.len(), 2, "one request per repository, independent of bound CR count");
        assert!(calls[0].1.iter().any(|argument| argument.contains("pr1:") && argument.contains("pr4:")));
        assert!(calls[1].1.iter().any(|argument| argument.contains("pr5:") && argument.contains("pr7:")));
    }

    #[tokio::test]
    async fn graphql_rate_limit_reports_budget_identity_and_reset() {
        let runner = Arc::new(MockRunner::new(vec![Ok(
            "HTTP/2 200 OK\r\nX-RateLimit-Reset: 1784736000\r\n\r\n{\"errors\":[{\"message\":\"API rate limit exceeded\"}]}".into(),
        )]));
        let api = Arc::new(GhApiClient::new(runner.clone()));
        let provider = GitHubChangeRequest::new("github".into(), "team/one".into(), api, runner);
        let error = provider.observe_bound(&[1]).await.expect_err("rate limited");
        assert!(error.contains("rate limited (budget=GraphQL, identity=host gh login, reset_at=2026-"), "{error}");
        assert!(crate::providers::github_api::rate_limit_reset(&error).is_some());
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
        let statuses = provider.observe_bound(&[1, 2]).await.expect("partial GraphQL result");
        assert!(!statuses.contains_key(&1));
        assert!(statuses.contains_key(&2));
    }
}
