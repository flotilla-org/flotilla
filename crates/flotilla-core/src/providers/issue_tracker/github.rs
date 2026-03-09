use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::providers::github_api::{clamp_per_page, GhApi};
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

fn parse_issue(provider_name: &str, v: &serde_json::Value) -> Option<(String, Issue)> {
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
    Some((
        id,
        Issue {
            title,
            labels,
            association_keys,
        },
    ))
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
    ) -> Result<Vec<(String, Issue)>, String> {
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

        let issues: Vec<(String, Issue)> = items
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
    ) -> Result<Vec<(String, Issue)>, String> {
        use futures::stream::{self, StreamExt};
        let futs: Vec<_> = ids
            .iter()
            .map(|id| {
                let endpoint = format!("repos/{}/issues/{}", self.repo_slug, id);
                let api = Arc::clone(&self.api);
                let repo_root = repo_root.to_path_buf();
                let provider_name = self.provider_name.clone();
                let id = id.clone();
                async move {
                    let body = api.get(&endpoint, &repo_root).await?;
                    let v: serde_json::Value =
                        serde_json::from_str(&body).map_err(|e| e.to_string())?;
                    parse_issue(&provider_name, &v)
                        .ok_or_else(|| format!("failed to parse issue {}", id))
                }
            })
            .collect();

        let results: Vec<_> = stream::iter(futs).buffer_unordered(10).collect().await;
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
    ) -> Result<Vec<(String, Issue)>, String> {
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

    async fn list_issues_changed_since(
        &self,
        repo_root: &Path,
        since: &str,
        per_page: usize,
    ) -> Result<IssueChangeset, String> {
        let per_page = clamp_per_page(per_page);
        let encoded_since = urlencoding::encode(since);
        let endpoint = format!(
            "repos/{}/issues?state=all&since={}&sort=updated&direction=desc&per_page={}",
            self.repo_slug, encoded_since, per_page
        );
        let response = self.api.get_with_headers(&endpoint, repo_root).await?;
        let items: Vec<serde_json::Value> =
            serde_json::from_str(&response.body).map_err(|e| e.to_string())?;

        let mut updated = Vec::new();
        let mut closed_ids = Vec::new();

        for v in &items {
            if v.as_object()
                .map(|o| o.contains_key("pull_request"))
                .unwrap_or(false)
            {
                continue;
            }
            let state = v["state"].as_str().unwrap_or("open");
            if state == "open" {
                if let Some(issue) = parse_issue(&self.provider_name, v) {
                    updated.push(issue);
                }
            } else if let Some(number) = v["number"].as_i64() {
                closed_ids.push(number.to_string());
            }
        }

        Ok(IssueChangeset {
            updated,
            closed_ids,
            has_more: response.has_next_page,
        })
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
    use super::*;
    use crate::providers::github_api::GhApiResponse;
    use crate::providers::issue_tracker::IssueTracker;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct MockGhApi {
        responses: Mutex<VecDeque<Result<GhApiResponse, String>>>,
    }

    impl MockGhApi {
        fn new(responses: Vec<Result<GhApiResponse, String>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl GhApi for MockGhApi {
        async fn get(&self, endpoint: &str, repo_root: &Path) -> Result<String, String> {
            self.get_with_headers(endpoint, repo_root)
                .await
                .map(|r| r.body)
        }
        async fn get_with_headers(
            &self,
            _endpoint: &str,
            _repo_root: &Path,
        ) -> Result<GhApiResponse, String> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockGhApi: no more responses")
        }
    }

    fn mock_tracker(responses: Vec<Result<GhApiResponse, String>>) -> GitHubIssueTracker {
        let api = Arc::new(MockGhApi::new(responses));
        let runner = Arc::new(crate::providers::testing::MockRunner::new(vec![]));
        GitHubIssueTracker::new("github".into(), "owner/repo".into(), api, runner)
    }

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

    #[tokio::test]
    async fn changed_since_partitions_open_and_closed() {
        let body = r#"[
            {"number": 1, "title": "Open issue", "state": "open", "labels": []},
            {"number": 2, "title": "Closed issue", "state": "closed", "labels": []},
            {"number": 3, "title": "Another open", "state": "open", "labels": []}
        ]"#;
        let tracker = mock_tracker(vec![Ok(GhApiResponse {
            status: 200,
            etag: None,
            body: body.to_string(),
            has_next_page: false,
            total_count: None,
        })]);

        let changeset = tracker
            .list_issues_changed_since(Path::new("/tmp/repo"), "2026-03-09T00:00:00Z", 50)
            .await
            .unwrap();

        assert_eq!(changeset.updated.len(), 2);
        assert_eq!(changeset.updated[0].0, "1");
        assert_eq!(changeset.updated[1].0, "3");
        assert_eq!(changeset.closed_ids, vec!["2"]);
        assert!(!changeset.has_more);
    }

    #[tokio::test]
    async fn changed_since_filters_pull_requests() {
        let body = r#"[
            {"number": 1, "title": "Issue", "state": "open", "labels": []},
            {"number": 2, "title": "PR", "state": "open", "labels": [], "pull_request": {"url": "..."}}
        ]"#;
        let tracker = mock_tracker(vec![Ok(GhApiResponse {
            status: 200,
            etag: None,
            body: body.to_string(),
            has_next_page: false,
            total_count: None,
        })]);

        let changeset = tracker
            .list_issues_changed_since(Path::new("/tmp/repo"), "2026-03-09T00:00:00Z", 50)
            .await
            .unwrap();

        assert_eq!(changeset.updated.len(), 1);
        assert_eq!(changeset.updated[0].0, "1");
        assert!(changeset.closed_ids.is_empty());
    }
}
