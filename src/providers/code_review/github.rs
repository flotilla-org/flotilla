use std::path::Path;
use async_trait::async_trait;
use serde::Deserialize;
use crate::providers::types::*;

pub struct GitHubCodeReview {
    provider_name: String,
}

#[derive(Debug, Deserialize)]
struct GhPr {
    number: i64,
    title: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    state: String,
    #[serde(default)]
    body: Option<String>,
}

use crate::providers::run_cmd;

impl GitHubCodeReview {
    pub fn new(provider_name: String) -> Self {
        Self { provider_name }
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
                let rest = &text[after..];
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('#') {
                    let num_str: String =
                        rest.chars().take_while(|c| c.is_ascii_digit()).collect();
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
        let mut correlation_keys = vec![
            CorrelationKey::Branch(pr.head_ref_name.clone()),
            CorrelationKey::ChangeRequestRef(self.provider_name.clone(), id.clone()),
        ];

        // Parse linked issues from title and body
        let texts = [pr.title.as_str(), pr.body.as_deref().unwrap_or("")];
        for text in texts {
            for issue_num in Self::parse_linked_issues(text) {
                let key =
                    CorrelationKey::IssueRef(self.provider_name.clone(), issue_num);
                if !correlation_keys.contains(&key) {
                    correlation_keys.push(key);
                }
            }
        }

        ChangeRequest {
            id,
            title: pr.title.clone(),
            branch: pr.head_ref_name.clone(),
            status: Self::parse_state(&pr.state),
            body: pr.body.clone(),
            correlation_keys,
        }
    }
}

#[async_trait]
impl super::CodeReview for GitHubCodeReview {
    fn display_name(&self) -> &str {
        "GitHub Pull Requests"
    }

    async fn list_change_requests(
        &self,
        repo_root: &Path,
        limit: usize,
    ) -> Result<Vec<ChangeRequest>, String> {
        let limit_str = limit.to_string();
        let output = run_cmd(
                "gh",
                &[
                    "pr",
                    "list",
                    "--json",
                    "number,title,headRefName,state,body",
                    "--limit",
                    &limit_str,
                ],
                repo_root,
            )
            .await?;
        let prs: Vec<GhPr> =
            serde_json::from_str(&output).map_err(|e| e.to_string())?;
        Ok(prs.iter().map(|pr| self.gh_pr_to_change_request(pr)).collect())
    }

    async fn get_change_request(
        &self,
        repo_root: &Path,
        id: &str,
    ) -> Result<ChangeRequest, String> {
        let output = run_cmd(
                "gh",
                &[
                    "pr",
                    "view",
                    id,
                    "--json",
                    "number,title,headRefName,state,body",
                ],
                repo_root,
            )
            .await?;
        let pr: GhPr =
            serde_json::from_str(&output).map_err(|e| e.to_string())?;
        Ok(self.gh_pr_to_change_request(&pr))
    }

    async fn open_in_browser(
        &self,
        repo_root: &Path,
        id: &str,
    ) -> Result<(), String> {
        run_cmd("gh", &["pr", "view", id, "--web"], repo_root)
            .await?;
        Ok(())
    }

    async fn list_merged_branch_names(
        &self,
        repo_root: &Path,
        limit: usize,
    ) -> Result<Vec<String>, String> {
        let limit_str = limit.to_string();
        let output = run_cmd(
                "gh",
                &[
                    "pr", "list", "--state", "merged", "--limit", &limit_str,
                    "--json", "headRefName",
                ],
                repo_root,
            )
            .await?;
        let prs: Vec<serde_json::Value> =
            serde_json::from_str(&output).map_err(|e| e.to_string())?;
        Ok(prs
            .iter()
            .filter_map(|p| p.get("headRefName").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect())
    }
}
