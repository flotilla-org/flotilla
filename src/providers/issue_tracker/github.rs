use std::path::Path;
use std::sync::Arc;
use async_trait::async_trait;
use crate::providers::types::*;
use crate::providers::github_api::{GhApiClient, clamp_per_page};
use crate::providers::run_cmd;

pub struct GitHubIssueTracker {
    provider_name: String,
    repo_slug: String,
    api: Arc<GhApiClient>,
}

impl GitHubIssueTracker {
    pub fn new(provider_name: String, repo_slug: String, api: Arc<GhApiClient>) -> Self {
        Self { provider_name, repo_slug, api }
    }
}

#[async_trait]
impl super::IssueTracker for GitHubIssueTracker {
    fn display_name(&self) -> &str {
        "GitHub Issues"
    }

    async fn list_issues(
        &self,
        repo_root: &Path,
        limit: usize,
    ) -> Result<Vec<Issue>, String> {
        let per_page = clamp_per_page(limit);
        let endpoint = format!(
            "repos/{}/issues?state=open&per_page={}",
            self.repo_slug, per_page
        );
        let body = self.api.get(&endpoint, repo_root).await?;
        let items: Vec<serde_json::Value> =
            serde_json::from_str(&body).map_err(|e| e.to_string())?;

        Ok(items
            .into_iter()
            .filter(|v| {
                // REST /issues includes PRs — filter them out
                !v.as_object()
                    .map(|o| o.contains_key("pull_request"))
                    .unwrap_or(false)
            })
            .filter_map(|v| {
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
                    self.provider_name.clone(),
                    id.clone(),
                )];
                Some(Issue {
                    id,
                    title,
                    labels,
                    association_keys,
                })
            })
            .collect())
    }

    async fn open_in_browser(
        &self,
        repo_root: &Path,
        id: &str,
    ) -> Result<(), String> {
        run_cmd("gh", &["issue", "view", id, "--web"], repo_root)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
