use crate::providers::github_api::{clamp_per_page, GhApiClient};
use crate::providers::types::*;
use crate::providers::CommandRunner;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

pub struct GitHubCodeReview {
    provider_name: String,
    repo_slug: String,
    api: Arc<GhApiClient>,
    runner: Arc<dyn CommandRunner>,
}

#[derive(Debug)]
struct GhPr {
    number: i64,
    title: String,
    head_ref_name: String,
    state: String,
    body: Option<String>,
    is_draft: bool,
}

impl GitHubCodeReview {
    pub fn new(
        provider_name: String,
        repo_slug: String,
        api: Arc<GhApiClient>,
        runner: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            provider_name,
            repo_slug,
            api,
            runner,
        }
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

    /// Parse "Fixes #N", "Closes #N", "Resolves #N" from text and return
    /// issue numbers found.
    fn parse_linked_issues(text: &str) -> Vec<String> {
        let mut issues = Vec::new();
        let lower = text.to_lowercase();
        for keyword in ["fixes", "closes", "resolves"] {
            let mut search_from = 0;
            while let Some(pos) = lower[search_from..].find(keyword) {
                let after = search_from + pos + keyword.len();
                let rest = &lower[after..];
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('#') {
                    let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if !num_str.is_empty() && !issues.contains(&num_str) {
                        issues.push(num_str);
                    }
                }
                search_from = after;
            }
        }
        issues
    }

    fn gh_pr_to_change_request(&self, pr: &GhPr) -> ChangeRequest {
        let id = pr.number.to_string();
        let correlation_keys = vec![
            CorrelationKey::Branch(pr.head_ref_name.clone()),
            CorrelationKey::ChangeRequestRef(self.provider_name.clone(), id.clone()),
        ];

        // Parse linked issues from title and body → association keys
        let mut association_keys: Vec<AssociationKey> = Vec::new();
        let texts = [pr.title.as_str(), pr.body.as_deref().unwrap_or("")];
        for text in texts {
            for issue_num in Self::parse_linked_issues(text) {
                let key = AssociationKey::IssueRef(self.provider_name.clone(), issue_num);
                if !association_keys.contains(&key) {
                    association_keys.push(key);
                }
            }
        }

        let status = if pr.state.to_uppercase() == "OPEN" && pr.is_draft {
            ChangeRequestStatus::Draft
        } else {
            Self::parse_state(&pr.state)
        };

        ChangeRequest {
            id,
            title: pr.title.clone(),
            branch: pr.head_ref_name.clone(),
            status,
            body: pr.body.clone(),
            correlation_keys,
            association_keys,
        }
    }
}

#[async_trait]
impl super::CodeReview for GitHubCodeReview {
    fn display_name(&self) -> &str {
        "GitHub Pull Requests"
    }

    fn section_label(&self) -> &str {
        "Pull Requests"
    }
    fn item_noun(&self) -> &str {
        "pull request"
    }
    fn abbreviation(&self) -> &str {
        "PR"
    }

    async fn list_change_requests(
        &self,
        repo_root: &Path,
        limit: usize,
    ) -> Result<Vec<ChangeRequest>, String> {
        let per_page = clamp_per_page(limit);
        let endpoint = format!(
            "repos/{}/pulls?state=open&per_page={}",
            self.repo_slug, per_page
        );
        let body = self.api.get(&endpoint, repo_root).await?;
        let items: Vec<serde_json::Value> =
            serde_json::from_str(&body).map_err(|e| e.to_string())?;

        Ok(items
            .iter()
            .filter_map(|v| {
                let number = v["number"].as_i64()?;
                let title = v["title"].as_str()?.to_string();
                let head_ref = v["head"]["ref"].as_str()?.to_string();
                let state = v["state"].as_str().unwrap_or("open").to_string();
                let body_text = v["body"].as_str().map(|s| s.to_string());
                let is_draft = v["draft"].as_bool().unwrap_or(false);

                let pr = GhPr {
                    number,
                    title,
                    head_ref_name: head_ref,
                    state,
                    body: body_text,
                    is_draft,
                };
                Some(self.gh_pr_to_change_request(&pr))
            })
            .collect())
    }

    async fn get_change_request(
        &self,
        repo_root: &Path,
        id: &str,
    ) -> Result<ChangeRequest, String> {
        let endpoint = format!("repos/{}/pulls/{}", self.repo_slug, id);
        let body = self.api.get(&endpoint, repo_root).await?;
        let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;

        let number = v["number"].as_i64().ok_or("missing number")?;
        let title = v["title"].as_str().ok_or("missing title")?.to_string();
        let head_ref = v["head"]["ref"]
            .as_str()
            .ok_or("missing head ref")?
            .to_string();
        let state = v["state"].as_str().unwrap_or("open").to_string();
        let body_text = v["body"].as_str().map(|s| s.to_string());
        let is_draft = v["draft"].as_bool().unwrap_or(false);

        let pr = GhPr {
            number,
            title,
            head_ref_name: head_ref,
            state,
            body: body_text,
            is_draft,
        };
        Ok(self.gh_pr_to_change_request(&pr))
    }

    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String> {
        self.runner
            .run("gh", &["pr", "view", id, "--web"], repo_root)
            .await?;
        Ok(())
    }

    async fn list_merged_branch_names(
        &self,
        repo_root: &Path,
        limit: usize,
    ) -> Result<Vec<String>, String> {
        let per_page = clamp_per_page(limit);
        let endpoint = format!(
            "repos/{}/pulls?state=closed&sort=updated&direction=desc&per_page={}",
            self.repo_slug, per_page
        );
        let body = self.api.get(&endpoint, repo_root).await?;
        let items: Vec<serde_json::Value> =
            serde_json::from_str(&body).map_err(|e| e.to_string())?;

        Ok(items
            .iter()
            .filter(|v| v["merged_at"].as_str().is_some())
            .filter_map(|v| v["head"]["ref"].as_str().map(|s| s.to_string()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_rest_api_pr_fields() {
        let json = r#"{
            "number": 42,
            "title": "Add feature",
            "head": {"ref": "feature-branch"},
            "state": "open",
            "body": "Fixes #7",
            "draft": true
        }"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v["number"].as_i64().unwrap(), 42);
        assert_eq!(v["head"]["ref"].as_str().unwrap(), "feature-branch");
        assert_eq!(v["draft"].as_bool().unwrap(), true);
    }

    #[test]
    fn parse_merged_pr_has_merged_at() {
        let json = r#"{
            "number": 10,
            "head": {"ref": "old-branch"},
            "state": "closed",
            "merged_at": "2026-01-01T00:00:00Z"
        }"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(v["merged_at"].as_str().is_some());
    }
}
