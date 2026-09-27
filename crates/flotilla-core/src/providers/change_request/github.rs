use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_resources::ChangeRequestStatus as ObservedChangeRequestStatus;

use crate::{
    change_request_observer::parse_gh_observation,
    providers::{
        gh_api_get,
        github_api::{clamp_per_page, parse_gh_api_response, rate_limit_error_from_response, GhApi},
        run, run_output,
        types::*,
        CommandRunner,
    },
};

fn execution_root() -> &'static Path {
    Path::new("/")
}

pub struct GitHubChangeRequest {
    provider_name: String,
    repo_slug: String,
    api: Arc<dyn GhApi>,
    runner: Arc<dyn CommandRunner>,
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
        Self { provider_name, repo_slug, api, runner }
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
    async fn observe_bound(&self, numbers: &[u64]) -> Result<HashMap<u64, ObservedChangeRequestStatus>, String> {
        if numbers.is_empty() {
            return Ok(HashMap::new());
        }
        let (owner, name) = self.repo_slug.split_once('/').ok_or("GitHub repository must have owner/name scope")?;
        let owner = serde_json::to_string(owner).map_err(|error| error.to_string())?;
        let name = serde_json::to_string(name).map_err(|error| error.to_string())?;
        let mut query = format!("query {{ repository(owner:{owner}, name:{name}) {{");
        for number in numbers {
            query.push_str(&format!(" pr{number}: pullRequest(number:{number}) {{ state isDraft headRefOid reviewDecision mergeable commits(last:1) {{ nodes {{ commit {{ statusCheckRollup {{ contexts(first:100) {{ nodes {{ ... on CheckRun {{ conclusion status }} ... on StatusContext {{ state }} }} }} }} }} }} }} }}"));
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
            let mut request = request.clone();
            request["statusCheckRollup"] = request["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"].clone();
            statuses.insert(*number, parse_gh_observation(&request.to_string(), observed_at)?);
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
        let endpoint = format!("repos/{}/pulls?state=all&per_page=100", self.repo_slug);
        let body = gh_api_get!(self.api, &endpoint, execution_root())?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        Ok(items
            .iter()
            .filter_map(Self::parse_pull_request)
            .find(|pull_request| pull_request.head_ref_name == branch)
            .map(|pull_request| self.gh_pr_to_change_request(&pull_request)))
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
