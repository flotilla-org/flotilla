use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::providers::github_api::{clamp_per_page, GhApi, GhApiResponse};
use crate::providers::types::*;
use crate::providers::CommandRunner;

pub struct GitHubIssueTracker {
    provider_name: String,
    repo_slug: String,
    api: Arc<dyn GhApi>,
    runner: Arc<dyn CommandRunner>,
}

impl GitHubIssueTracker {
    pub fn new(
        provider_name: String,
        repo_slug: String,
        api: Arc<dyn GhApi>,
        runner: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            provider_name,
            repo_slug,
            api,
            runner,
        }
    }
}

fn parse_issue(provider_name: &str, v: &serde_json::Value) -> Option<Issue> {
    let number = v["number"].as_i64()?;
    let title = v["title"].as_str()?.to_string();
    let labels: Vec<String> = v["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l["name"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let id = number.to_string();
    let association_keys = vec![AssociationKey::IssueRef(
        provider_name.to_string(),
        id.clone(),
    )];
    Some(Issue {
        id,
        title,
        labels,
        association_keys,
    })
}

#[async_trait]
impl super::IssueTracker for GitHubIssueTracker {
    fn display_name(&self) -> &str {
        "GitHub Issues"
    }

    async fn list_issues(&self, repo_root: &Path, limit: usize) -> Result<Vec<Issue>, String> {
        let page = self.list_issues_page(repo_root, 1, limit).await?;
        Ok(page.issues)
    }

    async fn list_issues_page(
        &self,
        repo_root: &Path,
        page: u32,
        per_page: usize,
    ) -> Result<IssuePage, String> {
        let per_page = clamp_per_page(per_page);
        let endpoint = format!(
            "repos/{}/issues?state=open&per_page={}&page={}",
            self.repo_slug, per_page, page
        );
        let response = self.api.get_with_headers(&endpoint, repo_root).await?;
        let items: Vec<serde_json::Value> =
            serde_json::from_str(&response.body).map_err(|e| e.to_string())?;

        let issues: Vec<Issue> = items
            .into_iter()
            .filter(|v| {
                !v.as_object()
                    .map(|o| o.contains_key("pull_request"))
                    .unwrap_or(false)
            })
            .filter_map(|v| parse_issue(&self.provider_name, &v))
            .collect();

        Ok(IssuePage {
            issues,
            total_count: None,
            has_more: response.has_next_page,
        })
    }

    async fn fetch_issues_by_id(
        &self,
        repo_root: &Path,
        ids: &[String],
    ) -> Result<Vec<Issue>, String> {
        let futs: Vec<_> = ids
            .iter()
            .map(|id| {
                let endpoint = format!("repos/{}/issues/{}", self.repo_slug, id);
                let api = Arc::clone(&self.api);
                let repo_root = repo_root.to_path_buf();
                let provider_name = self.provider_name.clone();
                async move {
                    let body = api.get(&endpoint, &repo_root).await?;
                    let v: serde_json::Value =
                        serde_json::from_str(&body).map_err(|e| e.to_string())?;
                    parse_issue(&provider_name, &v)
                        .ok_or_else(|| format!("failed to parse issue {}", id))
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;
        let mut issues = Vec::new();
        for result in results {
            match result {
                Ok(issue) => issues.push(issue),
                Err(e) => tracing::warn!("failed to fetch issue: {}", e),
            }
        }
        Ok(issues)
    }

    async fn search_issues(
        &self,
        repo_root: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Issue>, String> {
        let per_page = clamp_per_page(limit);
        let raw_query = format!("repo:{} is:issue is:open {}", self.repo_slug, query);
        let encoded_query = urlencoding::encode(&raw_query);
        let endpoint = format!("search/issues?q={}&per_page={}", encoded_query, per_page);
        let body = self.api.get(&endpoint, repo_root).await?;
        let response: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;

        let items = response["items"]
            .as_array()
            .ok_or("no items array in search response")?;
        Ok(items
            .iter()
            .filter_map(|v| parse_issue(&self.provider_name, v))
            .collect())
    }

    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String> {
        self.runner
            .run("gh", &["issue", "view", id, "--web"], repo_root)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_rest_api_issues_filters_pull_requests() {
        let json = r#"[
            {"number": 1, "title": "Bug report", "labels": [{"name": "bug"}]},
            {"number": 2, "title": "Feature PR", "labels": [], "pull_request": {"url": "..."}},
            {"number": 3, "title": "Enhancement", "labels": [{"name": "enhancement"}]}
        ]"#;
        let items: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        let filtered: Vec<&serde_json::Value> = items
            .iter()
            .filter(|v| !v.as_object().unwrap().contains_key("pull_request"))
            .collect();
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0]["number"], 1);
        assert_eq!(filtered[1]["number"], 3);
    }
}
