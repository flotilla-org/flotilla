use std::path::Path;
use async_trait::async_trait;
use serde::Deserialize;
use crate::providers::types::*;

pub struct GitHubIssueTracker {
    provider_name: String,
}

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: i64,
    title: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

impl GitHubIssueTracker {
    pub fn new(provider_name: String) -> Self {
        Self { provider_name }
    }

    async fn run_cmd(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Result<String, String> {
        let output = tokio::process::Command::new(cmd)
            .args(args)
            .current_dir(cwd)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).to_string())
        }
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
        let limit_str = limit.to_string();
        let output = self
            .run_cmd(
                "gh",
                &[
                    "issue",
                    "list",
                    "--json",
                    "number,title,labels",
                    "--limit",
                    &limit_str,
                    "--state",
                    "open",
                ],
                repo_root,
            )
            .await?;
        let issues: Vec<GhIssue> =
            serde_json::from_str(&output).map_err(|e| e.to_string())?;
        Ok(issues
            .into_iter()
            .map(|issue| {
                let id = issue.number.to_string();
                let correlation_keys = vec![CorrelationKey::IssueRef(
                    self.provider_name.clone(),
                    id.clone(),
                )];
                Issue {
                    id,
                    title: issue.title,
                    labels: issue.labels.into_iter().map(|l| l.name).collect(),
                    correlation_keys,
                }
            })
            .collect())
    }

    async fn open_in_browser(
        &self,
        repo_root: &Path,
        id: &str,
    ) -> Result<(), String> {
        self.run_cmd("gh", &["issue", "view", id, "--web"], repo_root)
            .await?;
        Ok(())
    }
}
