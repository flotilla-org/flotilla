use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::providers::{
    gh_api_get,
    github_api::{clamp_per_page, GhApi},
    run,
    types::*,
    CommandRunner,
};

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
    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        let per_page = clamp_per_page(limit);
        let endpoint = format!("repos/{}/pulls?state=open&per_page={}", self.repo_slug, per_page);
        let body = gh_api_get!(self.api, &endpoint, Path::new("/"))?;
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
        let body = gh_api_get!(self.api, &endpoint, Path::new("/"))?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        Ok(items
            .iter()
            .filter_map(Self::parse_pull_request)
            .find(|pull_request| pull_request.head_ref_name == branch)
            .map(|pull_request| self.gh_pr_to_change_request(&pull_request)))
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        let endpoint = format!("repos/{}/pulls/{}", self.repo_slug, id);
        let body = gh_api_get!(self.api, &endpoint, Path::new("/"))?;
        let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;

        let pr = Self::parse_pull_request(&v).ok_or("malformed pull request")?;
        Ok(self.gh_pr_to_change_request(&pr))
    }

    async fn get_change_request_for_admission(&self, id: &str) -> Result<super::ChangeRequestAdmission, String> {
        let endpoint = format!("repos/{}/pulls/{}", self.repo_slug, id);
        let body = gh_api_get!(self.api, &endpoint, Path::new("/"))?;
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        let pull_request = Self::parse_pull_request(&value).ok_or("malformed pull request")?;
        let (id, change_request) = self.gh_pr_to_change_request(&pull_request);
        Ok(super::ChangeRequestAdmission { id, change_request, base_ref: pull_request.base_ref_name })
    }

    async fn open_in_browser(&self, id: &str) -> Result<(), String> {
        run!(self.runner, "gh", &["pr", "view", id, "--repo", &self.repo_slug, "--web"], Path::new("/"))?;
        Ok(())
    }

    async fn close_change_request(&self, id: &str) -> Result<(), String> {
        run!(self.runner, "gh", &["pr", "close", id, "--repo", &self.repo_slug], Path::new("/"))?;
        Ok(())
    }

    async fn merge_change_request(&self, id: &str) -> Result<(), String> {
        run!(self.runner, "gh", &["pr", "merge", id, "--repo", &self.repo_slug, "--squash"], Path::new("/"))?;
        Ok(())
    }

    async fn list_merged_branch_names(&self, limit: usize) -> Result<Vec<String>, String> {
        let per_page = clamp_per_page(limit);
        let endpoint = format!("repos/{}/pulls?state=closed&sort=updated&direction=desc&per_page={}", self.repo_slug, per_page);
        let body = gh_api_get!(self.api, &endpoint, Path::new("/"))?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|e| e.to_string())?;

        Ok(items
            .iter()
            .filter(|v| v["merged_at"].as_str().is_some())
            .filter_map(|v| v["head"]["ref"].as_str().map(|s| s.to_string()))
            .collect())
    }
}
